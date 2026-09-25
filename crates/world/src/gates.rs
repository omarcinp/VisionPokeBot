//! Passages the story opens and closes, derived from the compiled events
//! (spec §5.1: "NPC blocker", "Warp ... or a flag if the script gates it").
//! Nothing here names a place of the game; every gate comes from what the
//! scripts do:
//!
//! - **Triggers that turn the player back.** A coord event whose script,
//!   on some path, walks the player off the tile (and neither warps nor
//!   switches the trigger off) blocks the tile while that path's conditions
//!   hold: the Saffron guards without tea, the Route 23 guards without the
//!   badge, the ghost of Pokémon Tower without the Silph Scope. The ways
//!   past are the conditions of the paths that let the player through, the
//!   map's entry scripts that switch the trigger's variable off (the gym
//!   door of Viridian unlocks once six badges are held), or, when nothing
//!   else does, the variable having moved on.
//! - **Tiles an entry script rewrites.** `setmetatile` in a map's
//!   `on_load`/`on_transition`/`on_resume` script, under the path's
//!   conditions: the Elite Four doors open once their member is beaten, the
//!   Silph Co. doors once unlocked, the Rocket Hideout stairs once the
//!   switch was found, the Victory Road barriers once a boulder is on the
//!   switch. A warp on a rewritten tile works only in the state that shows
//!   it.
//! - **Objects an entry script puts in the way** (`setobjectxyperm`): the
//!   old man lying across Viridian's north road.
//!
//! A gate's `ways` are alternatives (any one opens the tile), each a
//! conjunction of predicates; no ways means a wall. The conditions are
//! reduced against the blocking ones, so a way names only what matters
//! (`FLAG_SILPH_2F_DOOR_1`, not the state of the floor's other door).

use std::collections::{BTreeMap, BTreeSet};

use crate::events::{Condition, Effect, Events, ScriptPath, Val};
use crate::predicate::{check, BeliefView, CmpOp, Predicate, Requirement, Truth};
use crate::{MapData, World};

/// Alternatives kept per gate.
const MAX_WAYS: usize = 8;

/// Why a tile is gated.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GateKind {
    /// A coord event whose script turns the player back.
    Trigger { script: String },
    /// An entry script rewrites the tile's metatile.
    Metatile,
    /// An entry script moves an object onto the tile.
    Object { local_id: u32 },
}

/// One gated tile of a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub x: i32,
    pub y: i32,
    pub kind: GateKind,
    /// Any one of these opens the tile; empty means it never opens.
    pub ways: Vec<Requirement>,
    /// The warp on this tile works only in these states (a warp whose
    /// metatile an entry script rewrites); `None` when it isn't a warp tile
    /// rewritten by a script.
    pub warp_ways: Option<Vec<Requirement>>,
}

/// Gates per map, by tile.
pub type Gates = BTreeMap<String, BTreeMap<(i32, i32), Gate>>;

/// A literal of a path's conditions: a predicate, or its negation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Lit {
    /// Flags are kept as `Flag { is: true }` (badges too) with the value in
    /// `pos`; vars carry their comparison, `pos` always true.
    p: Predicate,
    pos: bool,
}

/// How a condition reads for gates.
enum Read {
    Lit(Lit),
    /// Script-local state (`VAR_TEMP_*`, `VAR_0x8004`, a symbol): dropped.
    Free,
    /// Something the player chooses or can assume away (a YES/NO answer, a
    /// menu choice, bag space, money): a blocking path that needs it is
    /// avoided, a passing path takes it.
    Choice,
}

/// Vars that live only while a script or a map visit runs.
pub fn is_local_var(var: &str) -> bool {
    var.starts_with("VAR_TEMP_")
        || var.starts_with("VAR_0x")
        || matches!(var, "VAR_RESULT" | "VAR_FACING" | "VAR_SPECIAL_4")
}

/// Flags that live only while a map is loaded.
pub fn is_local_flag(flag: &str) -> bool {
    flag.starts_with("FLAG_TEMP_") || flag == "0"
}

fn flag_lit(name: &str, is: bool) -> Read {
    if is_local_flag(name) {
        return Read::Free;
    }
    Read::Lit(Lit {
        p: Predicate::Flag {
            name: name.to_string(),
            is: true,
        },
        pos: is,
    })
}

/// The comparison of a condition, when it is against a number.
pub fn cmp_of(cmp: &crate::events::Cmp) -> Option<(CmpOp, i64)> {
    [
        (&cmp.eq, CmpOp::Eq),
        (&cmp.ne, CmpOp::Ne),
        (&cmp.lt, CmpOp::Lt),
        (&cmp.gt, CmpOp::Gt),
        (&cmp.le, CmpOp::Le),
        (&cmp.ge, CmpOp::Ge),
    ]
    .into_iter()
    .find_map(|(v, op)| v.as_ref().and_then(Val::as_int).map(|v| (op, v)))
}

fn read(c: &Condition) -> Read {
    match c {
        Condition::Flag { flag, is } => flag_lit(flag, *is),
        Condition::Trainer { trainer, defeated } => flag_lit(trainer, *defeated),
        Condition::Item { item, count, has } => Read::Lit(Lit {
            p: Predicate::HasItem {
                item: item.clone(),
                n: (*count).max(1) as u32,
            },
            pos: *has,
        }),
        Condition::Move { r#move, known } => Read::Lit(Lit {
            p: Predicate::PartyHasMove { mv: r#move.clone() },
            pos: *known,
        }),
        Condition::Var { var, .. } if is_local_var(var) => Read::Free,
        Condition::Var { var, cmp } => match cmp_of(cmp) {
            Some((op, value)) => Read::Lit(Lit {
                p: Predicate::Var {
                    name: var.clone(),
                    op,
                    value,
                },
                pos: true,
            }),
            None => Read::Free,
        },
        Condition::Answer { .. }
        | Condition::Choice { .. }
        | Condition::BagSpace { .. }
        | Condition::Money { .. }
        | Condition::Coins { .. }
        | Condition::PartySize { .. }
        | Condition::InParty { .. }
        | Condition::Special { .. }
        | Condition::ResultOf { .. }
        | Condition::Pokedex { .. }
        | Condition::PokedexComplete { .. } => Read::Choice,
        Condition::Other(_) => Read::Free,
    }
}

type Conj = Vec<Lit>;

/// The path's conditions as literals; `None` when `avoid_choices` and the
/// path needs a choice (a blocking path the player can stay off).
fn conj_of(when: &[Condition], avoid_choices: bool) -> Option<Conj> {
    let mut out = Vec::new();
    for c in when {
        match read(c) {
            Read::Lit(l) => {
                if !out.contains(&l) {
                    out.push(l);
                }
            }
            Read::Free => {}
            Read::Choice if avoid_choices => return None,
            Read::Choice => {}
        }
    }
    Some(out)
}

/// The inverse comparison (`== 3` → `!= 3`).
pub fn negate_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Ne,
        CmpOp::Ne => CmpOp::Eq,
        CmpOp::Lt => CmpOp::Ge,
        CmpOp::Ge => CmpOp::Lt,
        CmpOp::Gt => CmpOp::Le,
        CmpOp::Le => CmpOp::Gt,
    }
}

/// The values a var may take under comparisons, `None` when they conflict.
fn var_feasible(cmps: &[(CmpOp, i64)]) -> bool {
    let (mut lo, mut hi) = (i64::MIN / 2, i64::MAX / 2);
    let mut excluded = Vec::new();
    for &(op, v) in cmps {
        match op {
            CmpOp::Eq => {
                lo = lo.max(v);
                hi = hi.min(v);
            }
            CmpOp::Ne => excluded.push(v),
            CmpOp::Lt => hi = hi.min(v - 1),
            CmpOp::Le => hi = hi.min(v),
            CmpOp::Gt => lo = lo.max(v + 1),
            CmpOp::Ge => lo = lo.max(v),
        }
    }
    while lo <= hi && excluded.contains(&lo) {
        lo += 1;
    }
    while hi >= lo && excluded.contains(&hi) {
        hi -= 1;
    }
    lo <= hi
}

/// Whether two conjunctions can hold at once.
fn compatible(a: &[Lit], b: &[Lit]) -> bool {
    let mut vars: BTreeMap<&str, Vec<(CmpOp, i64)>> = BTreeMap::new();
    for l in a.iter().chain(b) {
        if let Predicate::Var { name, op, value } = &l.p {
            vars.entry(name).or_default().push((*op, *value));
        }
    }
    if !vars.values().all(|c| var_feasible(c)) {
        return false;
    }
    for x in a {
        for y in b {
            let same = match (&x.p, &y.p) {
                (Predicate::Flag { name: n1, .. }, Predicate::Flag { name: n2, .. }) => n1 == n2,
                (Predicate::HasItem { item: i1, .. }, Predicate::HasItem { item: i2, .. }) => {
                    i1 == i2
                }
                (Predicate::PartyHasMove { mv: m1 }, Predicate::PartyHasMove { mv: m2 }) => {
                    m1 == m2
                }
                _ => false,
            };
            if same && x.pos != y.pos {
                return false;
            }
        }
    }
    true
}

/// A conjunction as a requirement; negations the belief can't be asked
/// (not holding an item) are left out.
fn requirement(c: &[Lit]) -> Requirement {
    let mut out: Requirement = c
        .iter()
        .filter_map(|l| match (&l.p, l.pos) {
            (Predicate::Flag { name, .. }, is) => Some(Predicate::from_flag(name, is)),
            (p, true) => Some(p.clone()),
            (_, false) => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The ways `open` states hold, each reduced to the literals that tell it
/// apart from every `closed` state: `{D1, !D2}` against `{!D1, *}` is just
/// `{D1}`. Supersets of another way are dropped.
fn reduce(open: &[Conj], closed: &[Conj]) -> Vec<Requirement> {
    let mut ways: Vec<Requirement> = Vec::new();
    for c in open {
        let mut c = c.clone();
        let mut i = 0;
        while i < c.len() {
            let mut shorter = c.clone();
            shorter.remove(i);
            if closed.iter().any(|b| compatible(&shorter, b)) {
                i += 1;
            } else {
                c = shorter;
            }
        }
        let req = requirement(&c);
        if !ways.contains(&req) {
            ways.push(req);
        }
    }
    minimal(ways)
}

/// Values a var comparison is checked over when ways are merged (story
/// vars stay small).
const VAR_DOMAIN: i64 = 256;

/// Ways that each compare one var merge into one comparison when their
/// union is one (`== 1` or `>= 2` is `> 0`).
fn merge_var_ways(ways: Vec<Requirement>) -> Vec<Requirement> {
    let mut by_var: BTreeMap<String, Vec<(CmpOp, i64)>> = BTreeMap::new();
    let mut rest = Vec::new();
    for w in ways {
        match w.as_slice() {
            [Predicate::Var { name, op, value }] => {
                by_var.entry(name.clone()).or_default().push((*op, *value))
            }
            _ => rest.push(w),
        }
    }
    for (name, cmps) in by_var {
        let allowed: Vec<bool> = (0..VAR_DOMAIN)
            .map(|x| cmps.iter().any(|(op, v)| op.holds(x, *v)))
            .collect();
        let first = allowed.iter().position(|a| *a);
        let last = allowed.iter().rposition(|a| *a);
        let count = allowed.iter().filter(|a| **a).count() as i64;
        let merged = match (first, last) {
            (Some(f), Some(l))
                if count == l as i64 - f as i64 + 1 && l as i64 == VAR_DOMAIN - 1 =>
            {
                Some(if f == 0 {
                    None
                } else {
                    Some((CmpOp::Gt, f as i64 - 1))
                })
            }
            (Some(f), Some(l)) if count == l as i64 - f as i64 + 1 && f == 0 => {
                Some(Some((CmpOp::Le, l as i64)))
            }
            (Some(f), Some(l)) if f == l => Some(Some((CmpOp::Eq, f as i64))),
            _ if count == VAR_DOMAIN - 1 => allowed
                .iter()
                .position(|a| !*a)
                .map(|hole| Some((CmpOp::Ne, hole as i64))),
            _ => None,
        };
        match merged {
            Some(Some((op, value))) => rest.push(vec![Predicate::Var {
                name: name.clone(),
                op,
                value,
            }]),
            Some(None) => rest.push(Vec::new()),
            None => rest.extend(cmps.into_iter().map(|(op, value)| {
                vec![Predicate::Var {
                    name: name.clone(),
                    op,
                    value,
                }]
            })),
        }
    }
    rest
}

/// Drops ways that contain another way, sorts (what the player can do
/// before what the story state must be), and keeps [`MAX_WAYS`].
fn minimal(ways: Vec<Requirement>) -> Vec<Requirement> {
    let mut ways = merge_var_ways(ways);
    let vars = |w: &Requirement| {
        w.iter()
            .filter(|p| matches!(p, Predicate::Var { .. }))
            .count()
    };
    ways.sort_by(|a, b| (vars(a), a.len(), a).cmp(&(vars(b), b.len(), b)));
    let mut out: Vec<Requirement> = Vec::new();
    for w in ways {
        if !out.iter().any(|o| o.iter().all(|p| w.contains(p))) {
            out.push(w);
        }
    }
    out.truncate(MAX_WAYS);
    out
}

/// Whether the path walks the player off the tile.
fn moves_player(path: &ScriptPath) -> bool {
    path.does
        .iter()
        .any(|e| matches!(e, Effect::MovePlayer { move_player } if *move_player != (0, 0)))
}

fn warps(path: &ScriptPath) -> bool {
    path.does.iter().any(|e| matches!(e, Effect::Warp { .. }))
}

/// The value the path leaves `var` at, when it sets it to a number.
fn sets_var(path: &ScriptPath, var: &str) -> Option<i64> {
    path.does.iter().rev().find_map(|e| match e {
        Effect::Var { var: v, change } if v == var => change.eq.as_ref().and_then(Val::as_int),
        _ => None,
    })
}

/// The paths of the scripts a map runs when it is entered (`on_load`,
/// `on_transition`, `on_resume`), per script.
pub(crate) fn entry_scripts<'e>(events: &'e Events, map: &str) -> Vec<&'e [ScriptPath]> {
    let Some(ms) = events.map_scripts.get(map) else {
        return Vec::new();
    };
    ms.on_load
        .iter()
        .chain(&ms.on_transition)
        .chain(&ms.on_resume)
        .filter_map(|label| events.script(label))
        .map(|s| s.paths.as_slice())
        .collect()
}

/// Every gate of the world.
pub fn derive(world: &World) -> Gates {
    let mut gates: Gates = BTreeMap::new();
    let Some(events) = world.events() else {
        return gates;
    };
    // (var, value) pairs a trigger lets the player through with: the next
    // trigger of a gauntlet (the Route 23 guards) is armed by passing the
    // previous one on the same walk.
    let mut chained: Vec<(String, i64)> = Vec::new();
    for t in &events.triggers {
        let Some(script) = t.script.as_deref().and_then(|s| events.script(s)) else {
            continue;
        };
        for p in script.paths.iter().filter(|p| !moves_player(p)) {
            for e in &p.does {
                if let Effect::Var { var, change } = e {
                    if let Some(v) = change.eq.as_ref().and_then(Val::as_int) {
                        chained.push((var.clone(), v));
                    }
                }
            }
        }
    }
    chained.sort();
    chained.dedup();
    let mut names: Vec<&str> = world.maps().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    for name in names {
        let Some(map) = world.map(name) else { continue };
        let mut on_map: BTreeMap<(i32, i32), Gate> = BTreeMap::new();
        metatile_gates(events, map, &mut on_map);
        object_gates(events, map, &mut on_map);
        trigger_gates(events, map, &chained, &mut on_map);
        if !on_map.is_empty() {
            gates.insert(name.to_string(), on_map);
        }
    }
    gates
}

/// Tiles the map's entry scripts rewrite.
fn metatile_gates(events: &Events, map: &MapData, out: &mut BTreeMap<(i32, i32), Gate>) {
    for paths in entry_scripts(events, &map.name) {
        // The state each path leaves each tile it touches in.
        let states: Vec<BTreeMap<(i32, i32), bool>> = paths
            .iter()
            .map(|p| {
                let mut s = BTreeMap::new();
                for e in &p.does {
                    if let Effect::Metatile { metatile, open, .. } = e {
                        s.insert(*metatile, *open);
                    }
                }
                s
            })
            .collect();
        let mut tiles: Vec<(i32, i32)> = states.iter().flat_map(|s| s.keys().copied()).collect();
        tiles.sort_unstable();
        tiles.dedup();
        let conjs: Vec<Option<Conj>> = paths.iter().map(|p| conj_of(&p.when, false)).collect();
        for tile in tiles {
            if out.contains_key(&tile) || !map.in_bounds(tile.0, tile.1) {
                continue;
            }
            let base_open = map.tile(tile.0, tile.1).is_some_and(|t| t.collision == 0);
            let (mut open, mut closed) = (Vec::new(), Vec::new());
            let (mut plain, mut rewritten) = (Vec::new(), Vec::new());
            for (state, conj) in states.iter().zip(&conjs) {
                let Some(conj) = conj else { continue };
                let now = state.get(&tile).copied();
                if now.unwrap_or(base_open) {
                    open.push(conj.clone());
                } else {
                    closed.push(conj.clone());
                }
                if now.is_some() {
                    rewritten.push(conj.clone());
                } else {
                    plain.push(conj.clone());
                }
            }
            let is_warp = map.warps.iter().any(|w| (w.x, w.y) == tile);
            let warp_ways = is_warp.then(|| {
                if base_open {
                    // The script covers the warp (or its stairs) up.
                    if rewritten.is_empty() {
                        vec![Vec::new()]
                    } else {
                        reduce(&plain, &rewritten)
                    }
                } else if closed.is_empty() {
                    vec![Vec::new()]
                } else {
                    reduce(&open, &closed)
                }
            });
            if closed.is_empty() && base_open && warp_ways.is_none() {
                continue;
            }
            let ways = if closed.is_empty() {
                vec![Vec::new()]
            } else {
                reduce(&open, &closed)
            };
            out.insert(
                tile,
                Gate {
                    x: tile.0,
                    y: tile.1,
                    kind: GateKind::Metatile,
                    ways,
                    warp_ways,
                },
            );
        }
    }
}

/// Objects the map's entry scripts move onto a tile.
fn object_gates(events: &Events, map: &MapData, out: &mut BTreeMap<(i32, i32), Gate>) {
    for paths in entry_scripts(events, &map.name) {
        let placed: Vec<BTreeMap<u32, (i32, i32)>> = paths
            .iter()
            .map(|p| {
                let mut s = BTreeMap::new();
                for e in &p.does {
                    if let Effect::MoveObject { move_object, x, y } = e {
                        if let (Some(id), Some(x), Some(y)) =
                            (move_object.as_int(), x.as_int(), y.as_int())
                        {
                            if let Ok(id) = u32::try_from(id) {
                                s.insert(id, (x as i32, y as i32));
                            }
                        }
                    }
                }
                s
            })
            .collect();
        let mut spots: Vec<(u32, (i32, i32))> = placed
            .iter()
            .flat_map(|s| s.iter().map(|(k, v)| (*k, *v)))
            .collect();
        spots.sort_unstable();
        spots.dedup();
        let conjs: Vec<Option<Conj>> = paths.iter().map(|p| conj_of(&p.when, false)).collect();
        for (id, tile) in spots {
            if out.contains_key(&tile) || !map.in_bounds(tile.0, tile.1) {
                continue;
            }
            let Some(object) = map.objects.iter().find(|o| o.local_id == id) else {
                continue;
            };
            let (mut away, mut there) = (Vec::new(), Vec::new());
            for (s, conj) in placed.iter().zip(&conjs) {
                let Some(conj) = conj else { continue };
                if s.get(&id) == Some(&tile) {
                    there.push(conj.clone());
                } else {
                    away.push(conj.clone());
                }
            }
            let mut ways = reduce(&away, &there);
            if let Some(flag) = object.flag.as_deref().filter(|f| !is_local_flag(f)) {
                ways.push(vec![Predicate::from_flag(flag, true)]);
                ways = minimal(ways);
            }
            out.insert(
                tile,
                Gate {
                    x: tile.0,
                    y: tile.1,
                    kind: GateKind::Object { local_id: id },
                    ways,
                    warp_ways: None,
                },
            );
        }
    }
}

/// Coord events whose script turns the player back.
fn trigger_gates(
    events: &Events,
    map: &MapData,
    chained: &[(String, i64)],
    out: &mut BTreeMap<(i32, i32), Gate>,
) {
    for t in events.triggers.iter().filter(|t| t.map == map.name) {
        let Some(script) = t.script.as_deref().and_then(|s| events.script(s)) else {
            continue;
        };
        // The trigger fires while its var holds this value.
        let own = t.when.iter().find_map(|c| match c {
            Condition::Var { var, cmp } => cmp.eq.as_ref().and_then(Val::as_int).map(|v| (var, v)),
            _ => None,
        });
        let switches_off = |p: &ScriptPath| {
            own.is_some_and(|(var, v)| sets_var(p, var).is_some_and(|now| now != v))
        };
        // A scene the story arms (a var of its own, not a map visit's
        // local) that carries the player off elsewhere: the tile is not
        // walked past while it is armed, only onto (Oak stopping the
        // player at the edge of Pallet Town and taking them to his lab).
        let story_scene = own.is_some_and(|(var, _)| !is_local_var(var));
        let (mut blocking, mut passing) = (Vec::new(), Vec::new());
        for p in &script.paths {
            let turned_back = moves_player(p) && !warps(p) && !switches_off(p);
            let carried_off = story_scene && warps(p);
            if turned_back || carried_off {
                if let Some(c) = conj_of(&p.when, true) {
                    blocking.push(c);
                }
            } else if let Some(c) = conj_of(&p.when, false) {
                passing.push(c);
            }
        }
        if blocking.is_empty() {
            continue;
        }
        let mut ways = reduce(&passing, &blocking);
        if let Some((var, v)) = own {
            // The map's entry scripts switch the trigger off.
            for paths in entry_scripts(events, &map.name) {
                let (mut off, mut on) = (Vec::new(), Vec::new());
                for p in paths {
                    let Some(c) = conj_of(&p.when, false) else {
                        continue;
                    };
                    match sets_var(p, var) {
                        Some(now) if now != v => off.push(c),
                        _ => on.push(c),
                    }
                }
                if !off.is_empty() {
                    ways.extend(reduce(&off, &on));
                }
            }
            if !is_local_var(var) {
                // The trigger is off once the story has moved the var on.
                // One armed at the start (value 0) or by passing another
                // trigger (a gauntlet) is on until the story moves past it;
                // one a scene arms is on only in that scene's state.
                let armed = v == 0 || chained.iter().any(|(cv, cval)| cv == var && *cval == v);
                ways.push(vec![Predicate::Var {
                    name: var.clone(),
                    op: if armed { CmpOp::Gt } else { CmpOp::Ne },
                    value: v,
                }]);
            }
        }
        let tile = (t.x, t.y);
        let gate = Gate {
            x: t.x,
            y: t.y,
            kind: GateKind::Trigger {
                script: t.script.clone().unwrap_or_default(),
            },
            ways: minimal(ways),
            warp_ways: None,
        };
        match out.get_mut(&tile) {
            // Two gates on a tile: both must open.
            Some(g) => g.ways = both(&g.ways, &gate.ways),
            None => {
                out.insert(tile, gate);
            }
        }
    }
}

/// Whether a gate is open as a belief stands: some way holds (open), every
/// way fails (closed), or the belief can't tell.
pub fn gate_truth(gate: &Gate, belief: &dyn BeliefView) -> Truth {
    let mut unknown = false;
    for way in &gate.ways {
        let c = check(belief, way);
        if c.failed.is_empty() && c.unknown.is_empty() {
            return Truth::True;
        }
        unknown |= c.failed.is_empty();
    }
    if unknown {
        Truth::Unknown
    } else {
        Truth::False
    }
}

/// The gated tiles as a belief stands, for walking (the navigator): the
/// walkable tiles it knows closed (a trigger that turns the player back
/// while its scene is armed, an object a script put in the way) and the
/// walls it knows opened (a door a script unlocked). What the belief can't
/// tell is left as the map draws it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateTiles {
    pub closed: BTreeMap<String, BTreeSet<(i32, i32)>>,
    pub opened: BTreeMap<String, BTreeSet<(i32, i32)>>,
}

impl GateTiles {
    pub fn believed(world: &World, gates: &Gates, belief: &dyn BeliefView) -> GateTiles {
        let mut out = GateTiles::default();
        for (name, tiles) in gates {
            let Some(map) = world.map(name) else { continue };
            for (&tile, gate) in tiles {
                let base_open = map.tile(tile.0, tile.1).is_some_and(|t| t.collision == 0);
                match (base_open, gate_truth(gate, belief)) {
                    (true, Truth::False) => {
                        out.closed.entry(name.clone()).or_default().insert(tile);
                    }
                    (false, Truth::True) => {
                        out.opened.entry(name.clone()).or_default().insert(tile);
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Tiles of `map` known closed.
    pub fn closed_on(&self, map: &str) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.closed.get(map).into_iter().flatten().copied()
    }

    /// Walls of `map` known opened, as the path search takes them.
    pub fn opened_on(&self, map: &str) -> crate::path::Obstacles {
        self.opened
            .get(map)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }

    /// The same, with `tile` of `map` never closed: a walk may always end
    /// on a gate (stepping onto a trigger is what starts its scene).
    pub fn without(mut self, map: &str, tile: (i32, i32)) -> GateTiles {
        if let Some(t) = self.closed.get_mut(map) {
            t.remove(&tile);
        }
        self
    }
}

/// The ways through two gates at once: one way of each.
pub fn both(a: &[Requirement], b: &[Requirement]) -> Vec<Requirement> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let mut w = x.clone();
            w.extend(y.iter().cloned());
            w.sort();
            w.dedup();
            out.push(w);
        }
    }
    minimal(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(name: &str, pos: bool) -> Lit {
        Lit {
            p: Predicate::Flag {
                name: name.into(),
                is: true,
            },
            pos,
        }
    }

    #[test]
    fn ways_are_reduced_to_what_tells_them_apart() {
        // Silph Co. 2F: door 1 is open when its flag is set, whatever the
        // other door does.
        let open = vec![
            vec![flag("D1", true), flag("D2", false)],
            vec![flag("D1", true), flag("D2", true)],
        ];
        let closed = vec![
            vec![flag("D1", false), flag("D2", false)],
            vec![flag("D1", false), flag("D2", true)],
        ];
        assert_eq!(
            reduce(&open, &closed),
            vec![vec![Predicate::Flag {
                name: "D1".into(),
                is: true
            }]]
        );
        // Nothing closed: open without conditions.
        assert_eq!(reduce(&open, &[]), vec![Vec::<Predicate>::new()]);
    }

    #[test]
    fn var_comparisons_conflict_by_value() {
        let var = |op, value| Lit {
            p: Predicate::Var {
                name: "V".into(),
                op,
                value,
            },
            pos: true,
        };
        assert!(!compatible(&[var(CmpOp::Eq, 3)], &[var(CmpOp::Ne, 3)]));
        assert!(compatible(&[var(CmpOp::Ge, 2)], &[var(CmpOp::Eq, 5)]));
        assert!(!compatible(&[var(CmpOp::Lt, 2)], &[var(CmpOp::Ge, 2)]));
    }
}
