//! A warp taken in the dark: a fade that begins while the player stands on
//! a warp tile that fires is that warp, so the player is at its
//! destination. Perception tracks from there instead of matching the old
//! floor again (Switch, Rock Tunnel without Flash: the ladder at B1F
//! (33, 3) took the player to 1F (4, 2), the dim frames still matched B1F,
//! and the walk stepped off and on the ladder, up and down, until it
//! failed "looping").
//!
//! Only a warp stepped onto: arriving on one (a cave's mouth) and standing
//! there fires nothing, so the fades that follow an arrival say nothing
//! (Switch, Mt. Moon's exit to Route 4 (19, 5): each fade after arriving
//! put the belief back in the cave, and the walk out re-entered it).

use pokebot_state::{GameEvent, GameState, Observation, PlayerPose, ScreenState};
use pokebot_world::behavior::warp_fires;
use pokebot_world::World;

#[derive(Debug, Default)]
pub(crate) struct WarpTaken {
    /// The destination of the warp under the player when a fade began.
    to: Option<PlayerPose>,
    in_fade: bool,
    /// Where the player was when the current map was entered.
    entered: Option<PlayerPose>,
    /// The player has left that tile since.
    stepped: bool,
}

impl WarpTaken {
    pub(crate) fn observe(
        &mut self,
        world: Option<&World>,
        o: &Observation,
        state: &GameState,
    ) -> Vec<GameEvent> {
        let pose = state.player.pose.value.as_ref();
        if self.entered.as_ref().map(|p| &p.map) != pose.map(|p| &p.map) {
            self.entered = pose.cloned();
            self.stepped = false;
        } else if pose != self.entered.as_ref() {
            self.stepped = true;
        }
        if o.screen.value == ScreenState::Transition {
            if !self.in_fade {
                self.in_fade = true;
                self.to = world
                    .filter(|_| self.stepped)
                    .and_then(|w| destination(w, pose?, &state.world.paths_run));
            }
            return Vec::new();
        }
        self.in_fade = false;
        let Some(to) = self.to.take() else {
            return Vec::new();
        };
        // A battle's fade isn't a warp (none starts on a warp tile anyway).
        if o.battle.is_some() {
            return Vec::new();
        }
        vec![GameEvent::PlayerInferred {
            pose: to,
            candidates: Vec::new(),
        }]
    }
}

/// Where the warp on `pose`'s tile leads, if one is there and fires. An
/// elevator's door (`MAP_DYNAMIC`) leads where the last script path run
/// set it (`setdynamicwarp`, the floor picked at the panel): Switch,
/// Rocket Hideout, the door out to B4F left the belief in the car and
/// every walk waited on "locating the player".
pub(crate) fn destination(
    world: &World,
    pose: &PlayerPose,
    paths_run: &[(String, usize)],
) -> Option<PlayerPose> {
    let map = world.map(&pose.map)?;
    let tile = map.tile(pose.x, pose.y)?;
    if !warp_fires(tile.behavior) {
        return None;
    }
    let warp = map.warps.iter().find(|w| (w.x, w.y) == (pose.x, pose.y))?;
    if warp.dest_map == "MAP_DYNAMIC" {
        return dynamic_destination(world, paths_run);
    }
    let (to, x, y) = world.warp_destination(warp)?;
    Some(PlayerPose {
        map: to.name.clone(),
        x,
        y,
    })
}

/// The warp the latest path in `paths_run` that sets one set.
fn dynamic_destination(world: &World, paths_run: &[(String, usize)]) -> Option<PlayerPose> {
    use pokebot_world::events::{Effect, Val};
    let events = world.events()?;
    paths_run.iter().rev().find_map(|(script, path)| {
        let p = events.script(script)?.paths.get(*path)?;
        p.does.iter().rev().find_map(|e| {
            let Effect::SetWarp {
                set_warp,
                warp_id,
                x,
                y,
            } = e
            else {
                return None;
            };
            let dest = world.map(world.name_of(set_warp)?)?;
            let (x, y) = match (
                x.as_ref().and_then(Val::as_int),
                y.as_ref().and_then(Val::as_int),
            ) {
                (Some(x), Some(y)) => (x as i32, y as i32),
                _ => {
                    let w = dest
                        .warps
                        .get(usize::try_from(warp_id.as_ref()?.as_int()?).ok()?)?;
                    (w.x, w.y)
                }
            };
            Some(PlayerPose {
                map: dest.name.clone(),
                x,
                y,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{FrameMetrics, Knowledge, Observed};
    use std::path::Path;

    fn world() -> Option<World> {
        World::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")).ok()
    }

    fn frame(id: u64, screen: ScreenState) -> Observation {
        Observation::bare(
            id,
            Observed {
                value: screen,
                detector: "test".into(),
            },
            FrameMetrics::default(),
        )
    }

    fn at(map: &str, x: i32, y: i32) -> GameState {
        let mut state = GameState::default();
        state.player.pose = Knowledge::observed(
            PlayerPose {
                map: map.into(),
                x,
                y,
            },
            1,
        );
        state
    }

    /// Switch, Rock Tunnel: the fade on B1F's ladder (33, 3) lands on 1F
    /// (4, 2); a fade on a plain tile says nothing.
    #[test]
    fn a_fade_on_a_ladder_lands_at_its_destination() {
        let Some(world) = world() else { return };
        let ladder = at("RockTunnel_B1F", 33, 3);
        let mut w = WarpTaken::default();
        // Stepped onto it from the floor next to it.
        w.observe(
            Some(&world),
            &frame(0, ScreenState::Unknown),
            &at("RockTunnel_B1F", 33, 4),
        );
        assert!(w
            .observe(Some(&world), &frame(1, ScreenState::Transition), &ladder)
            .is_empty());
        assert!(w
            .observe(Some(&world), &frame(2, ScreenState::Transition), &ladder)
            .is_empty());
        let events = w.observe(Some(&world), &frame(3, ScreenState::Unknown), &ladder);
        assert_eq!(
            events,
            vec![GameEvent::PlayerInferred {
                pose: PlayerPose {
                    map: "RockTunnel_1F".into(),
                    x: 4,
                    y: 2,
                },
                candidates: Vec::new(),
            }]
        );
        // Once per fade.
        assert!(w
            .observe(Some(&world), &frame(4, ScreenState::Unknown), &ladder)
            .is_empty());
        // A fade elsewhere on the floor: nothing.
        let floor = at("RockTunnel_B1F", 33, 5);
        let mut w = WarpTaken::default();
        w.observe(Some(&world), &frame(1, ScreenState::Transition), &floor);
        assert!(w
            .observe(Some(&world), &frame(2, ScreenState::Unknown), &floor)
            .is_empty());
    }

    /// Switch, Rocket Hideout: B4F picked at the elevator's panel, the car's
    /// door (a `MAP_DYNAMIC` warp) leads to B4F, not nowhere.
    #[test]
    fn an_elevator_door_leads_where_the_panel_set_it() {
        let Some(world) = world() else { return };
        let events = world.events().unwrap();
        let panel = "RocketHideout_Elevator_EventScript_FloorSelect";
        let to_b4f = events
            .script(panel)
            .unwrap()
            .paths
            .iter()
            .position(|p| {
                p.does.iter().any(|e| {
                    matches!(e,
                    pokebot_world::events::Effect::SetWarp { set_warp, .. }
                        if set_warp == "MAP_ROCKET_HIDEOUT_B4F")
                })
            })
            .unwrap();
        let map = world.map("RocketHideout_Elevator").unwrap();
        let door = map
            .warps
            .iter()
            .find(|w| {
                w.dest_map == "MAP_DYNAMIC"
                    && map.tile(w.x, w.y).is_some_and(|t| warp_fires(t.behavior))
            })
            .unwrap();
        let pose = PlayerPose {
            map: "RocketHideout_Elevator".into(),
            x: door.x,
            y: door.y,
        };
        assert_eq!(destination(&world, &pose, &[]), None);
        let run = [
            ("PalletTown_EventScript_Sign".to_string(), 0),
            (panel.to_string(), to_b4f),
        ];
        let to = destination(&world, &pose, &run).expect("B4F");
        assert_eq!(to.map, "RocketHideout_B4F");
        let b4f = world.map("RocketHideout_B4F").unwrap();
        assert!(b4f.in_bounds(to.x, to.y), "{to}");
    }

    /// Switch: leaving Mt. Moon puts the player on Route 4's cave mouth
    /// (19, 5), a warp. The fades after arriving aren't that warp: the
    /// belief stays on Route 4. Stepping off and back on is.
    #[test]
    fn arriving_on_a_warp_is_not_taking_it() {
        let Some(world) = world() else { return };
        let mouth = at("Route4", 19, 5);
        assert!(destination(&world, mouth.player.pose.value.as_ref().unwrap(), &[]).is_some());
        let mut w = WarpTaken::default();
        let inside = at("MtMoon_1F", 18, 37);
        w.observe(Some(&world), &frame(0, ScreenState::Unknown), &inside);
        // Arrived: the first frames on Route 4, then a fade.
        w.observe(Some(&world), &frame(1, ScreenState::Unknown), &mouth);
        w.observe(Some(&world), &frame(2, ScreenState::Transition), &mouth);
        assert!(w
            .observe(Some(&world), &frame(3, ScreenState::Unknown), &mouth)
            .is_empty());
        // Off it and back on: taken.
        let p = mouth.player.pose.value.clone().unwrap();
        w.observe(
            Some(&world),
            &frame(4, ScreenState::Unknown),
            &at(&p.map, p.x, p.y + 1),
        );
        w.observe(Some(&world), &frame(5, ScreenState::Unknown), &mouth);
        w.observe(Some(&world), &frame(6, ScreenState::Transition), &mouth);
        assert_eq!(
            w.observe(Some(&world), &frame(7, ScreenState::Unknown), &mouth)
                .len(),
            1
        );
    }
}
