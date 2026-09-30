//! The Nugget Bridge farm: money from the Team Rocket grunt at the north
//! end of Route 24 (`Route24/scripts.inc`). Past the five contest
//! trainers, his trigger (`Route24_EventScript_RocketTrigger*`) hands over
//! a NUGGET *before* the battle, and only a win sets
//! `VAR_MAP_SCENE_ROUTE24` to 1: lost, the trigger stays armed and the
//! NUGGET is kept through the white-out. So the farm:
//!
//! 1. **Prepare**: beats the contest trainers still standing, heals at the
//!    Cerulean Center (the white-out's respawn), stores every member but
//!    the one that loses to the grunt cheapest ([`keeper`]) in the PC,
//!    and saves in front of the grunt.
//! 2. **Farm**: walks onto the trigger, takes the NUGGET, loses the battle
//!    on purpose ([`BattlePlan::Lose`]), is healed at the Center after the
//!    white-out, and again, until `nuggets` were received; saving every
//!    `save_every`. A white-out costs `4 × top level × badge multiplier`
//!    (`ComputeWhiteOutMoneyLoss`): the keeper alone keeps it small
//!    (Lv6 with two badges: ¥144 a NUGGET worth ¥5000).
//! 3. **Sell**: the NUGGETs at the nearest mart, as many as fit under
//!    ¥999,999.
//! 4. **Restore**: the stored members back into the party.
//!
//! Progress is kept in a farm file written right after each in-game save,
//! so a restart (a reload of that save) resumes where the save left off.
//! The one save-scum the user allows is this farm's white-outs: they are
//! the plan, not a failure, and never reload the save.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_planner::Combatant;
use pokebot_state::{GameEvent, GameState, Pocket, ScreenState};
use serde::{Deserialize, Serialize};

use crate::battle::{harmless, losing_move, BattleMemory};
use crate::party::{display_name, Party};
use crate::tools::battle::BattleStep;
use crate::tools::{
    progress, BattlePlan, Dest, Expects, Intent, StepContext, ToolContext, ToolError, ToolStep,
    SETTLE_FRAMES,
};
use crate::{Action, Decision, Expectation, Outcome};

pub const MAP: &str = "Route24";
/// The grunt handing over the NUGGET.
pub const ROCKET: &str = "TRAINER_TEAM_ROCKET_GRUNT_6";
/// Set to 1 when the grunt is beaten: the farm is over for good.
pub const SCENE_VAR: &str = "VAR_MAP_SCENE_ROUTE24";
/// The five contest trainers, south to north as the bridge is walked.
pub const CONTEST: [&str; 5] = [
    "TRAINER_BUG_CATCHER_CALE",
    "TRAINER_LASS_ALI",
    "TRAINER_YOUNGSTER_TIMMY",
    "TRAINER_LASS_RELI",
    "TRAINER_CAMPER_ETHAN",
];
/// The Center the white-out brings the player back to.
pub const CENTER: &str = "CeruleanCity_PokemonCenter_1F";
pub const NUGGET: &str = "ITEM_NUGGET";
/// The keeper's chance to beat the grunt with its losing move, at most.
pub const MAX_KEEPER_WIN: f64 = 0.01;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FarmPhase {
    #[default]
    Prepare,
    Farm,
    Sell,
    Restore,
    Done,
}

/// The farm's progress as of the last in-game save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FarmFile {
    pub phase: FarmPhase,
    /// The member kept in the party to lose with.
    #[serde(default)]
    pub keeper: Option<String>,
    /// Species stored in the PC for the farm, to take back.
    #[serde(default)]
    pub deposited: Vec<String>,
    /// NUGGETs received (and saved).
    #[serde(default)]
    pub farmed: u32,
    /// Money the white-outs cost, as the battle text told it.
    #[serde(default)]
    pub lost: u64,
    #[serde(default)]
    pub sold: u32,
    #[serde(default)]
    pub earned: u64,
}

impl FarmFile {
    /// The file at `path`, or a new farm when there is none.
    pub fn load(path: &Path) -> Result<Self, ToolError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ToolError::Failed(format!("{}: {e}", path.display()))),
        }
    }

    pub fn store(&self, path: &Path) -> Result<(), ToolError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", dir.display())))?;
        }
        let text = serde_json::to_string_pretty(self).expect("farm file serializes");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))
    }
}

#[derive(Debug, Clone)]
pub struct FarmConfig {
    /// NUGGETs to receive in all (the farm file counts them across runs).
    pub nuggets: u32,
    /// White-outs between in-game saves.
    pub save_every: u32,
    pub file: PathBuf,
}

/// The party member to lose with, and its chance to win anyway: the one
/// with the least chance against the grunt using only the move
/// [`losing_move`] picks (none with a [`harmless`] move), then the lowest
/// level (the white-out's cost is the top level's); `None` when every
/// member could win.
pub fn keeper(data: &GameData, party: &Party) -> Option<(u8, f64)> {
    let memory = BattleMemory {
        trainer: true,
        ..BattleMemory::default()
    };
    party
        .members
        .iter()
        .map(|m| {
            let mut alone = m.clone();
            if alone.moves.iter().all(|mv| mv == "?") {
                alone.moves = data.default_moves(&m.species, m.level);
            }
            let single = Party {
                members: vec![alone],
            };
            let p_win = match losing_move(data, &single, &memory) {
                Some((_, mv)) if harmless(data, &mv) => 0.0,
                Some((_, mv)) => Combatant::new(
                    data,
                    &m.species,
                    m.level,
                    vec![mv],
                    pokebot_planner::prepare::OUR_IV,
                )
                .and_then(|c| pokebot_planner::battle_vs_trainer(data, &[c], ROCKET))
                .map_or(1.0, |e| e.p_win),
                // No PP at all: STRUGGLE, priced as a win to be safe.
                None => 1.0,
            };
            (m.slot, m.level, p_win)
        })
        .filter(|(_, _, p)| *p <= MAX_KEEPER_WIN)
        .min_by(|a, b| a.2.total_cmp(&b.2).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)))
        .map(|(slot, _, p)| (slot, p))
}

/// NUGGETs in the bag, as the belief holds them.
pub fn nuggets_held(state: &GameState) -> Option<u32> {
    let items = state.bag.pockets.get(&Pocket::Items)?.value.as_ref()?;
    Some(
        items
            .iter()
            .filter(|(item, _)| item == NUGGET)
            .map(|(_, n)| u32::from(*n))
            .sum(),
    )
}

/// The tile south of the grunt's trigger the farm steps onto (the right
/// one: he turns to face the player, and nobody walks).
fn stand_tile(ctx: &ToolContext<'_>) -> Result<(i32, i32), ToolError> {
    let map = ctx
        .world
        .map(MAP)
        .ok_or_else(|| ToolError::Failed(format!("no {MAP} in the world")))?;
    let t = map
        .triggers
        .iter()
        .filter(|t| t.var.as_deref() == Some(SCENE_VAR))
        .max_by_key(|t| t.x)
        .ok_or_else(|| ToolError::Failed(format!("{MAP} has no {SCENE_VAR} trigger")))?;
    Ok((t.x, t.y + 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LosePhase {
    /// The last step, onto the trigger.
    Approach,
    /// The grunt's pages ("…you just earned a fabulous prize!", the
    /// NUGGET, "…join TEAM ROCKET?") before the battle.
    Scene,
    Battle,
    /// The white-out's black pages, then the nurse healing the party.
    WhiteOut,
}

/// One round: onto the trigger, the NUGGET, the battle lost, the white-out
/// and the nurse; done back in the Center's overworld.
struct LoseStep {
    phase: LosePhase,
    /// Facing the trigger, and when the step onto it was sent.
    turned: bool,
    stepped_at: Option<u64>,
    battle: BattleStep,
    /// The last action came from the battle step (its outcome goes back).
    battle_acted: bool,
    got_nugget: bool,
    battle_seen: bool,
    /// The nurse spoke after the white-out (the party healed).
    nurse_seen: bool,
    whited_out: bool,
    last_press: Option<u64>,
}

/// Frames between A presses on screens without a ▼ to wait for (the
/// white-out pages).
const PRESS_GAP: u64 = 40;
/// Frames after the step onto the trigger for its scene to start.
const TRIGGER_FRAMES: u64 = 300;
/// A hold long enough for one step (a tap only turns); the trigger's
/// script stops the player on its tile.
const STEP_HOLD: std::time::Duration = std::time::Duration::from_millis(250);

impl LoseStep {
    fn press_a(&mut self, frame: u64, label: &str) -> Decision {
        if self.last_press.is_some_and(|f| frame < f + PRESS_GAP) {
            return Decision::Wait(format!("{label}: waiting before A"));
        }
        self.last_press = Some(frame);
        Decision::Act(Action::new(
            label,
            vec![ControllerCommand::Press(Button::A)],
            Expectation::InputsDone,
            10,
        ))
    }
}

impl ToolStep for LoseStep {
    fn expects(&self) -> Expects {
        Expects::LOSING
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if std::mem::take(&mut self.battle_acted) {
            self.battle.on_outcome(action, outcome, ctx);
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        let page = o
            .dialogue
            .as_ref()
            .map(|d| d.lines.join(" "))
            .unwrap_or_default();
        if page.contains("received a NUGGET") {
            self.got_nugget = true;
        }
        if page.contains("don't have any room") {
            return Decision::Fail("the bag has no room for a NUGGET".into());
        }
        if o.screen.value == ScreenState::Whiteout {
            self.whited_out = true;
            self.phase = LosePhase::WhiteOut;
            return self.press_a(o.frame_id, "white-out: next page");
        }
        if o.battle.is_some() {
            self.battle_seen = true;
            self.phase = LosePhase::Battle;
        }
        match self.phase {
            LosePhase::Approach => {
                if o.dialogue.is_some() {
                    self.phase = LosePhase::Scene;
                    return Decision::Wait("the grunt's scene".into());
                }
                if !std::mem::replace(&mut self.turned, true) {
                    return Decision::Act(Action::new(
                        "face the trigger",
                        vec![ControllerCommand::Press(Button::Up)],
                        Expectation::InputsDone,
                        20,
                    ));
                }
                match self.stepped_at {
                    None => {
                        self.stepped_at = Some(o.frame_id);
                        Decision::Act(Action::new(
                            "step onto the trigger",
                            vec![ControllerCommand::Hold {
                                buttons: Button::Up.into(),
                                duration: STEP_HOLD,
                            }],
                            Expectation::DialogueOpen,
                            120,
                        ))
                    }
                    Some(at) if o.frame_id > at + TRIGGER_FRAMES => Decision::Fail(format!(
                        "stepped onto the trigger and nothing happened: {SCENE_VAR} is set \
                         (the grunt was beaten; the farm is over)"
                    )),
                    Some(_) => Decision::Wait("the trigger's scene".into()),
                }
            }
            LosePhase::Scene => {
                if o.dialogue.is_some() {
                    return crate::new_game::advance_or_wait(o.dialogue.as_ref(), "the grunt");
                }
                Decision::Wait("the battle is coming".into())
            }
            LosePhase::Battle => {
                // A lost battle ends in the white-out; back in the
                // overworld without one, it was won.
                if o.battle.is_none() && o.player.is_some() && o.dialogue.is_none() {
                    return Decision::Fail(format!(
                        "the battle with {ROCKET} ended without a white-out: won, and \
                         the farm is over"
                    ));
                }
                let d = self.battle.next(ctx);
                self.battle_acted = matches!(d, Decision::Act(_));
                match d {
                    // "battle over" in the overworld: see above.
                    Decision::Done(_) => Decision::Wait("after the battle".into()),
                    d => d,
                }
            }
            LosePhase::WhiteOut => {
                if o.dialogue.is_some() {
                    self.nurse_seen = true;
                    return crate::new_game::advance_or_wait(o.dialogue.as_ref(), "the nurse");
                }
                // The Center isn't located yet: the pose is tracked from
                // the grunt's tile, and every Center 1F looks alike. The
                // healing over and the screen quiet, the step ends; the
                // respawn is pinned from the belief after it.
                let quiet = o.menu.is_none()
                    && o.battle.is_none()
                    && !matches!(
                        o.screen.value,
                        ScreenState::Whiteout | ScreenState::Transition
                    )
                    && ctx.quiet_frames >= SETTLE_FRAMES;
                if self.nurse_seen && quiet {
                    return Decision::Done(if self.got_nugget {
                        "a NUGGET, then the white-out".into()
                    } else {
                        "the white-out, without a NUGGET read".into()
                    });
                }
                Decision::Wait("back to the Center".into())
            }
        }
    }
}

/// One round at the grunt from the tile south of the trigger; whether the
/// NUGGET was received.
fn lose_once(ctx: &mut ToolContext<'_>) -> Result<bool, ToolError> {
    let mut step = LoseStep {
        phase: LosePhase::Approach,
        turned: false,
        stepped_at: None,
        battle: BattleStep::new(Arc::clone(&ctx.data), BattlePlan::Lose, true),
        battle_acted: false,
        got_nugget: false,
        battle_seen: false,
        nurse_seen: false,
        whited_out: false,
        last_press: None,
    };
    let summary = ctx.drive(&mut step)?;
    // What the game did: the party fainted, healed at the Center.
    if step.battle_seen {
        ctx.emit(GameEvent::BattleEnded)?;
    }
    ctx.emit(GameEvent::WhitedOut)?;
    ctx.emit(GameEvent::Healed)?;
    if let Some(pose) = crate::goal::respawn_pose(ctx) {
        ctx.runtime.clear_pose_hint();
        ctx.emit(GameEvent::PlayerLocated { pose: pose.clone() })?;
        ctx.runtime.set_pose_hint(pose);
    }
    ctx.emit(progress("NuggetFarm", summary))?;
    Ok(step.got_nugget)
}

/// Saves in-game, then records the farm as of that save.
fn save(ctx: &mut ToolContext<'_>, file: &FarmFile, path: &Path) -> Result<(), ToolError> {
    ctx.invoke(&Intent::Save).result?;
    file.store(path)
}

fn species_of(state: &GameState) -> Vec<String> {
    state
        .party
        .value
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|m| m.species.value.clone().unwrap_or_default())
        .collect()
}

/// The contest trainers beaten, the respawn at Cerulean, the party down to
/// the keeper.
fn prepare(ctx: &mut ToolContext<'_>, file: &mut FarmFile) -> Result<(), ToolError> {
    if ctx
        .state()
        .world
        .var(SCENE_VAR)
        .value
        .is_some_and(|v| v != 0)
    {
        return Err(ToolError::Failed(format!(
            "{SCENE_VAR} is set: the grunt was beaten, no NUGGET to farm"
        )));
    }
    for trainer in CONTEST {
        if ctx.state().world.flag(trainer).value == Some(true) {
            continue;
        }
        let object = crate::tools::trainer_object(&ctx.world, MAP, trainer)
            .ok_or_else(|| ToolError::Failed(format!("no object on {MAP} fights {trainer}")))?;
        ctx.info(format!("nugget farm: beating {trainer}"));
        ctx.invoke(&Intent::Beat {
            trainer: trainer.into(),
            map: MAP.into(),
            object,
        })
        .result?;
    }
    // The white-out brings the player back to the last Center healed at.
    ctx.invoke(&Intent::Heal {
        center: Some(CENTER.into()),
    })
    .result?;
    park(ctx, file)
}

/// Stores every member but the keeper; records what was stored.
fn park(ctx: &mut ToolContext<'_>, file: &mut FarmFile) -> Result<(), ToolError> {
    let party = Party::from_state(ctx.state());
    if party.members.is_empty() {
        ctx.invoke(&Intent::Probe {
            fact: crate::tools::ProbeFact::Party,
        })
        .result?;
    }
    let party = Party::from_state(ctx.state());
    let (slot, p_win) = keeper(&ctx.data, &party).ok_or_else(|| {
        ToolError::Failed(format!(
            "every member could beat {ROCKET}: no one to lose with (a win ends the farm)"
        ))
    })?;
    let species = species_of(ctx.state());
    let kept = species.get(usize::from(slot)).cloned().unwrap_or_default();
    ctx.emit(progress(
        "NuggetFarm",
        format!(
            "{} loses to the grunt (P(win) {:.2} %): the others go to the PC",
            display_name(&kept),
            p_win * 100.0
        ),
    ))?;
    let others: Vec<u8> = (0..species.len() as u8).filter(|s| *s != slot).collect();
    if !others.is_empty() {
        crate::tools::pc::deposit(ctx, &others)?;
    }
    file.keeper = Some(kept);
    file.deposited.extend(
        others
            .iter()
            .filter_map(|s| species.get(usize::from(*s)).cloned()),
    );
    Ok(())
}

/// Before [`ROCKET`] is fought: the farm, while it can still be run. A
/// win against him ends it for good, so the goal loop farms first. `None`
/// when there's nothing to farm: done, the grunt already beaten, or every
/// member of a known party able to beat him (none to lose with).
pub fn before_the_grunt(
    ctx: &mut ToolContext<'_>,
    config: &FarmConfig,
) -> Result<Option<String>, ToolError> {
    let file = FarmFile::load(&config.file)?;
    if file.phase == FarmPhase::Done {
        return Ok(None);
    }
    if file.phase == FarmPhase::Prepare {
        if ctx
            .state()
            .world
            .var(SCENE_VAR)
            .value
            .is_some_and(|v| v != 0)
        {
            return Ok(None);
        }
        let party = Party::from_state(ctx.state());
        if !party.members.is_empty() && keeper(&ctx.data, &party).is_none() {
            ctx.info(format!(
                "nugget farm: every member could beat {ROCKET}; no farm"
            ));
            return Ok(None);
        }
    }
    run(ctx, config).map(Some)
}

/// Runs the farm from the phase its file is at. What it did, when done.
pub fn run(ctx: &mut ToolContext<'_>, config: &FarmConfig) -> Result<String, ToolError> {
    let path = config.file.as_path();
    let mut file = FarmFile::load(path)?;
    let stand = stand_tile(ctx)?;
    loop {
        ctx.info(format!(
            "nugget farm: {:?}, {} of {} NUGGETs",
            file.phase, file.farmed, config.nuggets
        ));
        match file.phase {
            FarmPhase::Prepare => {
                prepare(ctx, &mut file)?;
                // Saved in front of the grunt.
                ctx.invoke(&Intent::Go {
                    dest: Dest::Tile {
                        map: MAP.into(),
                        x: stand.0,
                        y: stand.1,
                    },
                })
                .result?;
                file.phase = FarmPhase::Farm;
                save(ctx, &file, path)?;
            }
            FarmPhase::Farm => {
                // A reload before the stored members were saved gone.
                if species_of(ctx.state()).len() > 1 {
                    park(ctx, &mut file)?;
                    save(ctx, &file, path)?;
                }
                // The scheduler's heals and audits aren't wanted: the
                // white-out heals.
                let scheduling = std::mem::replace(&mut ctx.scheduler.enabled, false);
                let farmed = farm(ctx, &mut file, config, stand);
                ctx.scheduler.enabled = scheduling;
                farmed?;
                file.phase = FarmPhase::Sell;
                save(ctx, &file, path)?;
            }
            FarmPhase::Sell => {
                let held = nuggets_held(ctx.state()).filter(|n| *n > 0).unwrap_or(999);
                let (sold, earned) = crate::tools::sell::sell(ctx, NUGGET, held)?;
                file.sold += sold;
                file.earned += u64::from(earned);
                file.phase = FarmPhase::Restore;
                save(ctx, &file, path)?;
            }
            FarmPhase::Restore => {
                let take = std::mem::take(&mut file.deposited);
                if let Err(e) = crate::tools::pc::withdraw(ctx, &take) {
                    file.deposited = take;
                    return Err(e);
                }
                file.phase = FarmPhase::Done;
                save(ctx, &file, path)?;
            }
            FarmPhase::Done => {
                return Ok(format!(
                    "{} NUGGETs farmed (white-outs cost ¥{}), {} sold for ¥{}, the party back",
                    file.farmed, file.lost, file.sold, file.earned
                ))
            }
        }
    }
}

/// The rounds, saving every `save_every` NUGGETs.
fn farm(
    ctx: &mut ToolContext<'_>,
    file: &mut FarmFile,
    config: &FarmConfig,
    stand: (i32, i32),
) -> Result<(), ToolError> {
    let path = config.file.as_path();
    let mut unsaved = 0u32;
    let mut lost = 0u64;
    while file.farmed + unsaved < config.nuggets {
        ctx.invoke(&Intent::Go {
            dest: Dest::Tile {
                map: MAP.into(),
                x: stand.0,
                y: stand.1,
            },
        })
        .result?;
        let money = ctx.state().money.value;
        if !lose_once(ctx)? {
            return Err(ToolError::Failed(
                "the round ended without the NUGGET being read".into(),
            ));
        }
        unsaved += 1;
        if let (Some(before), Some(after)) = (money, ctx.state().money.value) {
            lost += u64::from(before.saturating_sub(after));
        }
        ctx.emit(progress(
            "NuggetFarm",
            format!(
                "NUGGET {} of {} ({unsaved} unsaved)",
                file.farmed + unsaved,
                config.nuggets
            ),
        ))?;
        if unsaved >= config.save_every.max(1) || file.farmed + unsaved >= config.nuggets {
            let mut saved = file.clone();
            saved.farmed += unsaved;
            saved.lost += lost;
            save(ctx, &saved, path)?;
            *file = saved;
            unsaved = 0;
            lost = 0;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
    }

    fn member(d: &GameData, slot: u8, species: &str, level: u8, moves: &[&str]) -> Member {
        Member {
            slot,
            moves: moves.iter().map(|m| (*m).to_owned()).collect(),
            ..Member::new(d, species, level)
        }
    }

    /// The emulator scenario's party at the Cerulean Center (bridge
    /// trainers beaten): PARAS Lv6 with SCRATCH only scratches at the
    /// grunt's Lv15 EKANS and ZUBAT in vain; GEODUDE Lv19 could win.
    #[test]
    fn the_keeper_is_the_one_that_cannot_win() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![
                member(
                    &d,
                    0,
                    "SPECIES_GEODUDE",
                    19,
                    &["MOVE_TACKLE", "MOVE_DEFENSE_CURL", "MOVE_ROCK_THROW"],
                ),
                member(&d, 1, "SPECIES_CLEFAIRY", 9, &["MOVE_POUND", "MOVE_GROWL"]),
                member(
                    &d,
                    2,
                    "SPECIES_ZUBAT",
                    10,
                    &["MOVE_LEECH_LIFE", "MOVE_SUPERSONIC"],
                ),
                member(
                    &d,
                    3,
                    "SPECIES_IVYSAUR",
                    24,
                    &["MOVE_TACKLE", "MOVE_VINE_WHIP", "MOVE_RAZOR_LEAF"],
                ),
                member(&d, 4, "SPECIES_PARAS", 6, &["MOVE_SCRATCH"]),
            ],
        };
        let (slot, p) = keeper(&d, &party).expect("a keeper");
        // Neither CLEFAIRY (GROWL does nothing) nor PARAS can win: the
        // lower level makes each white-out cheaper.
        assert_eq!(slot, 4, "p {p}");
        assert_eq!(p, 0.0);
        // Only strong members: nobody to lose with.
        let strong = Party {
            members: vec![member(&d, 0, "SPECIES_IVYSAUR", 40, &["MOVE_RAZOR_LEAF"])],
        };
        assert_eq!(keeper(&d, &strong), None);
    }

    #[test]
    fn the_losing_move_does_nothing_when_it_can() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![member(
                &d,
                0,
                "SPECIES_ZUBAT",
                10,
                &["MOVE_LEECH_LIFE", "MOVE_SUPERSONIC"],
            )],
        };
        let memory = BattleMemory::default();
        // SUPERSONIC confuses (the foe may hit itself), priced like a
        // 40-power attack: LEECH LIFE's 20 is the weaker harm.
        assert_eq!(
            losing_move(&d, &party, &memory).map(|(_, m)| m),
            Some("MOVE_LEECH_LIFE".into())
        );
        let party = Party {
            members: vec![member(
                &d,
                0,
                "SPECIES_CLEFAIRY",
                9,
                &["MOVE_POUND", "MOVE_GROWL"],
            )],
        };
        assert_eq!(
            losing_move(&d, &party, &memory).map(|(_, m)| m),
            Some("MOVE_GROWL".into())
        );
    }

    #[test]
    fn the_farm_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("pokebot-farm-{}", std::process::id()));
        let path = dir.join("farm.json");
        assert_eq!(FarmFile::load(&path).unwrap(), FarmFile::default());
        let file = FarmFile {
            phase: FarmPhase::Farm,
            keeper: Some("SPECIES_PARAS".into()),
            deposited: vec!["SPECIES_GEODUDE".into()],
            farmed: 12,
            ..FarmFile::default()
        };
        file.store(&path).unwrap();
        assert_eq!(FarmFile::load(&path).unwrap(), file);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
