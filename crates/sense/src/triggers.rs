//! A trigger's trainer battle that fires while the belief has the trigger
//! done: the game shows its var still armed. Whatever path the belief
//! recorded for it is retracted, the var becomes what the trigger fires
//! on, and its trainers are not beaten (Switch: a wild battle was taken
//! for Cerulean's rival battle and saved; every reload planned the walk
//! north first, met the rival unprepared and fainted).

use pokebot_state::{DialogueKind, GameEvent, GameState, Observation, PlayerPose};
use pokebot_world::events::{Condition, Effect, Val};
use pokebot_world::World;

/// Where the player stood when a battle began, until its first page says
/// whether it is a wild one.
#[derive(Debug, Default)]
pub(crate) struct TriggerBattle {
    in_battle: bool,
    from: Option<PlayerPose>,
}

impl TriggerBattle {
    pub(crate) fn observe(
        &mut self,
        world: Option<&World>,
        o: &Observation,
        state: &GameState,
    ) -> Vec<GameEvent> {
        if o.battle.is_none() {
            if o.player.is_some() {
                self.in_battle = false;
                self.from = None;
            }
            return Vec::new();
        }
        if !self.in_battle {
            self.in_battle = true;
            self.from = state.player.pose.value.clone();
        }
        let Some(page) = o
            .dialogue
            .as_ref()
            .filter(|d| d.kind == DialogueKind::BattleText && !d.lines.is_empty())
            .map(|d| d.lines.join(" "))
        else {
            return Vec::new();
        };
        let Some(pose) = self.from.take() else {
            return Vec::new();
        };
        // "Wild X appeared!": a wild battle, whatever tile it began on.
        if page.starts_with("Wild ") {
            return Vec::new();
        }
        world.map_or_else(Vec::new, |w| rearmed(w, state, &pose))
    }
}

/// The facts a trainer battle beginning at `pose` contradicts: a battle
/// trigger on that tile whose var the belief holds moved on.
pub(crate) fn rearmed(world: &World, state: &GameState, pose: &PlayerPose) -> Vec<GameEvent> {
    let Some(events) = world.events() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in events
        .triggers
        .iter()
        .filter(|t| t.map == pose.map && (t.x, t.y) == (pose.x, pose.y))
    {
        let Some(name) = t.script.as_deref() else {
            continue;
        };
        let Some(script) = events.script(name) else {
            continue;
        };
        let fights = script.paths.iter().any(|p| {
            p.does
                .iter()
                .any(|e| matches!(e, Effect::Battle { rematch: false, .. }))
        });
        let armed = t.when.iter().find_map(|c| match c {
            Condition::Var { var, cmp } => cmp
                .eq
                .as_ref()
                .and_then(Val::as_int)
                .and_then(|v| u16::try_from(v).ok())
                .map(|v| (var.clone(), v)),
            _ => None,
        });
        let Some((var, value)) = armed.filter(|_| fights) else {
            continue;
        };
        let believed = state.world.vars.get(&var).and_then(|k| k.value);
        if believed.is_none_or(|v| v == value) {
            continue;
        }
        for (s, path) in state.world.paths_run.iter().filter(|(s, _)| s == name) {
            let Some(p) = script.paths.get(*path) else {
                continue;
            };
            let mut flags = Vec::new();
            let mut vars = Vec::new();
            for e in &p.does {
                match e {
                    Effect::Set { set: f } | Effect::Clear { clear: f } => flags.push(f.clone()),
                    Effect::Defeated { defeated } => {
                        flags.push(defeated.clone());
                        out.push(GameEvent::FlagObserved {
                            flag: defeated.clone(),
                            value: false,
                        });
                    }
                    Effect::Var { var, .. } if !vars.contains(var) => vars.push(var.clone()),
                    _ => {}
                }
            }
            out.insert(
                0,
                GameEvent::ScriptPathRetracted {
                    script: s.clone(),
                    path: *path,
                    flags,
                    vars,
                },
            );
        }
        out.push(GameEvent::VarObserved { var, value });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{
        BattleObservation, DialogueObservation, FrameMetrics, Knowledge, Observed, Region,
        ScreenState,
    };
    use std::path::Path;

    fn world() -> Option<World> {
        World::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")).ok()
    }

    fn battle(frame: u64, page: Option<&str>) -> Observation {
        let mut o = Observation::bare(
            frame,
            Observed {
                value: ScreenState::BattleText,
                detector: "test".into(),
            },
            FrameMetrics::default(),
        );
        o.battle = Some(BattleObservation {
            menu: None,
            player_name: None,
            player_level: None,
            player_hp_numbers: None,
            opponent_name: None,
            opponent_level: None,
            player_hp: None,
            opponent_hp: None,
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: None,
            opponent_shiny: None,
            level_up_stats: None,
        });
        o.dialogue = page.map(|p| DialogueObservation {
            kind: DialogueKind::BattleText,
            region: Region::new(8, 118, 224, 36),
            waiting_for_input: true,
            arrow: None,
            stable_frames: 20,
            text_cells: Vec::new(),
            lines: vec![p.to_owned()],
            help: false,
        });
        o
    }

    /// Switch: the belief had Cerulean's rival beaten (a wild battle taken
    /// for his, and saved); on the trigger tile his battle begins anyway.
    #[test]
    fn a_trigger_battle_the_belief_had_done_retracts_it() {
        let Some(world) = world().filter(|w| w.events().is_some()) else {
            return;
        };
        let script = "CeruleanCity_EventScript_RivalTriggerLeft";
        let var = "VAR_MAP_SCENE_CERULEAN_CITY_RIVAL";
        let mut state = GameState::default();
        state.player.pose = Knowledge::observed(
            PlayerPose {
                map: "CeruleanCity".into(),
                x: 22,
                y: 6,
            },
            1,
        );
        state
            .world
            .vars
            .insert(var.into(), Knowledge::tracked(1, Some(1)));
        state.world.record_path(script, 4);
        let beaten = world.events().unwrap().script(script).unwrap().paths[4]
            .does
            .iter()
            .find_map(|e| match e {
                Effect::Defeated { defeated } => Some(defeated.clone()),
                _ => None,
            })
            .expect("path 4 fights the rival");

        // A wild battle on the tile says nothing.
        let mut t = TriggerBattle::default();
        assert!(t.observe(Some(&world), &battle(2, None), &state).is_empty());
        let wild = battle(3, Some("Wild SPEAROW appeared!"));
        assert!(t.observe(Some(&world), &wild, &state).is_empty());

        // The rival's: the path goes, the var is armed, he isn't beaten.
        let mut t = TriggerBattle::default();
        assert!(t.observe(Some(&world), &battle(2, None), &state).is_empty());
        let rival = battle(3, Some("RIVAL TERRY would like to battle!"));
        let events = t.observe(Some(&world), &rival, &state);
        assert!(
            matches!(&events[0], GameEvent::ScriptPathRetracted { script: s, path: 4, .. } if s == script),
            "{events:?}"
        );
        assert!(events.contains(&GameEvent::FlagObserved {
            flag: beaten,
            value: false
        }));
        assert_eq!(
            events.last(),
            Some(&GameEvent::VarObserved {
                var: var.into(),
                value: 0
            })
        );
        // Once per battle.
        assert!(t.observe(Some(&world), &rival, &state).is_empty());

        // A belief that holds the trigger armed: nothing to correct.
        state
            .world
            .vars
            .insert(var.into(), Knowledge::tracked(0, Some(1)));
        let mut t = TriggerBattle::default();
        t.observe(Some(&world), &battle(2, None), &state);
        assert!(t.observe(Some(&world), &rival, &state).is_empty());
    }
}
