//! Where Dig and the Escape Rope lead: the save's `escapeWarp`. The game
//! sets it in `UpdateEscapeWarp` (src/overworld.c) when a warp leads from
//! an outdoor map (`IsMapTypeOutdoors`) into one that isn't, unless the
//! player leaves Viridian Forest, to the outdoor tile of the warp taken,
//! one tile south unless the player faced south. A map may set it on
//! arrival instead (`setescapewarp` in its `OnTransition`: Berry Forest,
//! Pattern Bush). An escape therefore goes back out the way in; it never
//! crosses a cave.

use pokebot_state::{EscapeWarp, PlayerPose};

use crate::behavior::{arrow_warp, stair_warp};
use crate::events::{Cmp, Condition, Effect, Val};
use crate::predicate::CmpOp;
use crate::World;

/// The one outdoor map whose warps into buildings leave the escape warp
/// alone (its exits lead to the gates of the Route 2 it was entered from).
pub const VIRIDIAN_FOREST: &str = "ViridianForest";

/// How a map change moves the escape warp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapeChange {
    Keep,
    Set(EscapeWarp),
    /// It moved, to somewhere not known.
    Lost,
}

/// Whether a warp from `from` to `to` sets the escape warp (the map
/// types' rule of `UpdateEscapeWarp`; unknown maps count as setting it).
pub fn warp_sets_escape(world: &World, from: &str, to: &str) -> bool {
    match (world.map(from), world.map(to)) {
        (Some(a), Some(b)) => a.is_outdoor() && !b.is_outdoor() && from != VIRIDIAN_FOREST,
        _ => true,
    }
}

/// Whether arriving on `map` sets the escape warp by script.
pub fn sets_escape_on_arrival(world: &World, map: &str) -> bool {
    escape_scripts(world, map).next().is_some()
}

/// The `setescapewarp` effects of `map`'s transition scripts, with the
/// conditions of their paths.
fn escape_scripts<'w>(
    world: &'w World,
    map: &str,
) -> impl Iterator<Item = (&'w [Condition], &'w Effect)> + 'w {
    let events = world.events();
    let labels = events
        .and_then(|e| e.map_scripts.get(map))
        .map(|ms| ms.on_transition.as_slice())
        .unwrap_or(&[]);
    labels
        .iter()
        .filter_map(move |l| events?.script(l))
        .flat_map(|s| &s.paths)
        .filter_map(|p| {
            let e = p
                .does
                .iter()
                .find(|e| matches!(e, Effect::EscapeWarp { .. }))?;
            Some((p.when.as_slice(), e))
        })
}

fn holds(cmp: &Cmp, v: i64) -> bool {
    [
        (&cmp.eq, CmpOp::Eq),
        (&cmp.ne, CmpOp::Ne),
        (&cmp.lt, CmpOp::Lt),
        (&cmp.gt, CmpOp::Gt),
        (&cmp.le, CmpOp::Le),
        (&cmp.ge, CmpOp::Ge),
    ]
    .into_iter()
    .all(|(val, op)| {
        val.as_ref()
            .is_none_or(|x| x.as_int().is_some_and(|x| op.holds(v, x)))
    })
}

/// What arriving at `pose` sets by script: the path whose conditions the
/// arrival tile meets (`getplayerxy` puts it in VAR_TEMP_1 and 2: Pattern
/// Bush escapes by the side it was entered from). `None` when the map
/// sets nothing; `Lost` when no single path is known to run.
fn arrival_escape(world: &World, pose: &PlayerPose) -> Option<EscapeChange> {
    let mut any = false;
    let mut found: Vec<EscapeWarp> = Vec::new();
    for (when, effect) in escape_scripts(world, &pose.map) {
        any = true;
        let Effect::EscapeWarp {
            escape_warp, x, y, ..
        } = effect
        else {
            continue;
        };
        let met = when.iter().all(|c| match c {
            Condition::Var { var, cmp } if var == "VAR_TEMP_1" => holds(cmp, i64::from(pose.x)),
            Condition::Var { var, cmp } if var == "VAR_TEMP_2" => holds(cmp, i64::from(pose.y)),
            _ => false,
        });
        let at = (
            x.as_ref().and_then(Val::as_int),
            y.as_ref().and_then(Val::as_int),
        );
        if let (true, Some(map), (Some(x), Some(y))) = (met, world.name_of(escape_warp), at) {
            let e = EscapeWarp {
                map: map.to_owned(),
                x: x as i32,
                y: y as i32,
                entered: pose.map.clone(),
            };
            if !found.contains(&e) {
                found.push(e);
            }
        }
    }
    if !any {
        return None;
    }
    Some(match <[EscapeWarp; 1]>::try_from(found) {
        Ok([e]) => EscapeChange::Set(e),
        Err(_) => EscapeChange::Lost,
    })
}

/// How the player's move from `last` (the last tile seen on the old map)
/// to `arrived` changes the escape warp. A warp that sets it is taken on
/// its tile: the escape is that tile, one south unless the player walked
/// south into it (`UpdateEscapeWarp`: `y - 7 + (facing != DIR_SOUTH)`).
/// Mt. Moon's mouth on Route 4 (19, 5), walked into from below, escapes to
/// (19, 6). A setting warp the player wasn't seen next to (tracking lost
/// across it) leaves it unknown; maps no warp joins (a reload) keep it.
pub fn after_map_change(world: &World, last: &PlayerPose, arrived: &PlayerPose) -> EscapeChange {
    if last.map == arrived.map {
        return EscapeChange::Keep;
    }
    if let Some(change) = arrival_escape(world, arrived) {
        return change;
    }
    if !warp_sets_escape(world, &last.map, &arrived.map) {
        return EscapeChange::Keep;
    }
    let (Some(from), Some(to)) = (world.map(&last.map), world.map(&arrived.map)) else {
        return EscapeChange::Lost;
    };
    let near = |x: i32, y: i32| (x - last.x).abs() + (y - last.y).abs();
    let warp = from
        .warps
        .iter()
        .filter(|w| world.name_of(&w.dest_map) == Some(to.name.as_str()))
        .min_by_key(|w| near(w.x, w.y));
    let Some(w) = warp else {
        return EscapeChange::Keep;
    };
    if near(w.x, w.y) > 2 {
        return EscapeChange::Lost;
    }
    let behavior = from.tile(w.x, w.y).map(|t| t.behavior).unwrap_or(0);
    let south = if (last.x, last.y) != (w.x, w.y) {
        last.y < w.y
    } else {
        arrow_warp(behavior)
            .or(stair_warp(behavior))
            .is_some_and(|d| d == pokebot_state::Direction::Down)
    };
    EscapeChange::Set(EscapeWarp {
        map: from.name.clone(),
        x: w.x,
        y: w.y + i32::from(!south),
        entered: to.name.clone(),
    })
}
