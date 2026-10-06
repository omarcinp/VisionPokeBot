//! Field moves (spec §7.2 `Surf`, `Cut`, `Strength`, `RockSmash`, `Fly`,
//! plus Flash and Waterfall): used generically, the way a player would.
//!
//! Two paths, both closed-loop on the screen:
//!
//! * **Overworld prompt** (Cut, Rock Smash, Strength, Surf, Waterfall):
//!   stand facing the obstacle, press A, and answer YES to the question
//!   that names the move ("This tree looks like it can be CUT down! …
//!   Would you like to CUT it?"). The move worked once a page says
//!   "<mon> used <MOVE>" (Strength: "… made it possible to move
//!   boulders"); a page without the question means the game didn't offer
//!   it (no badge, or nobody knows the move).
//! * **Party menu** (Flash, Fly, and the fallback for the others): Start →
//!   POKéMON → the member that knows the move (party knowledge) → the
//!   move's row in the action window (field moves print in blue there) →
//!   for Fly, the region map: the cursor is walked cell by cell to the
//!   destination's cell and A flies there.
//!
//! Strength boulders are pushed one tile per step, each push verified by
//! the player moving into the boulder's old tile.

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::{Direction, GameEvent, GameState, Observation};
use pokebot_vision::detect::fly_map;

use super::go::{GoStep, NavParts};
use super::menu::{
    open_start_menu, pick_row, Closer, MenuRow, Retries, CURSOR_FRAMES, MENU_FRAMES, SCREEN_FRAMES,
};
use super::{
    progress, Dest, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep,
};
use crate::bag::fits;
use crate::nav::{direction_button, Destination};
use crate::{Action, Decision, Expectation, Outcome};

/// Frames without dialogue after the last page before an overworld prompt
/// counts as over.
const PROMPT_SETTLE_FRAMES: u32 = 40;
/// Frames without dialogue after YES before the move's "used" page is
/// given up on (its cut-in plays first).
const USED_PAGE_FRAMES: u32 = 420;
/// After the last page the move's cut-in (a black band across the screen
/// with the Pokémon, animated) and the obstacle's own animation play; a
/// step onto the tree's tile before `removeobject` bumps. The move counts
/// as done once the screen has been still (few changed pixels) for this
/// many frames in a row, the player located …
const STILL_FRAMES: u32 = 20;
const STILL_PIXELS: u32 = 300;
/// … or, with wanderers never letting the screen rest, this long after
/// the last page.
const ANIMATION_MAX_FRAMES: u64 = 300;
/// A cut tree's or smashed rock's tile matching the map render this well
/// (per mille) is free.
const OBSTACLE_GONE_SCORE: u32 = 850;
/// Frames a question's text must stay unchanged before it is answered.
pub const QUESTION_PRINTED_FRAMES: u32 = 8;
/// A presses that opened no dialogue before the prompt fails.
const MAX_PRESS_RETRIES: u32 = 3;
/// Frames the region map may stay up after A before the press is retried.
const FLY_TAKEOFF_FRAMES: u64 = 150;

/// The field moves the tools use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMove {
    Cut,
    Fly,
    Surf,
    Strength,
    Flash,
    RockSmash,
    Waterfall,
    /// Out of a cave or building to the escape warp ([`super::escape`]).
    Dig,
}

impl FieldMove {
    pub const ALL: [FieldMove; 8] = [
        FieldMove::Cut,
        FieldMove::Fly,
        FieldMove::Surf,
        FieldMove::Strength,
        FieldMove::Flash,
        FieldMove::RockSmash,
        FieldMove::Waterfall,
        FieldMove::Dig,
    ];

    /// `MOVE_CUT` → `Cut`.
    pub fn from_move(mv: &str) -> Option<FieldMove> {
        FieldMove::ALL.into_iter().find(|f| f.move_id() == mv)
    }

    pub fn move_id(self) -> &'static str {
        match self {
            FieldMove::Cut => "MOVE_CUT",
            FieldMove::Fly => "MOVE_FLY",
            FieldMove::Surf => "MOVE_SURF",
            FieldMove::Strength => "MOVE_STRENGTH",
            FieldMove::Flash => "MOVE_FLASH",
            FieldMove::RockSmash => "MOVE_ROCK_SMASH",
            FieldMove::Waterfall => "MOVE_WATERFALL",
            FieldMove::Dig => "MOVE_DIG",
        }
    }

    /// The move's name as the game prints it (party menu row, messages).
    pub fn label(self) -> &'static str {
        match self {
            FieldMove::Cut => "CUT",
            FieldMove::Fly => "FLY",
            FieldMove::Surf => "SURF",
            FieldMove::Strength => "STRENGTH",
            FieldMove::Flash => "FLASH",
            FieldMove::RockSmash => "ROCK SMASH",
            FieldMove::Waterfall => "WATERFALL",
            FieldMove::Dig => "DIG",
        }
    }

    /// The badge that allows the move outside battle (FireRed:
    /// `FLAG_BADGE0n_GET` checked by the field scripts and the party menu);
    /// none for Dig, which comes after the HMs the party menu checks.
    pub fn badge(self) -> Option<u8> {
        match self {
            FieldMove::Flash => Some(1),
            FieldMove::Cut => Some(2),
            FieldMove::Fly => Some(3),
            FieldMove::Strength => Some(4),
            FieldMove::Surf => Some(5),
            FieldMove::RockSmash => Some(6),
            FieldMove::Waterfall => Some(7),
            FieldMove::Dig => None,
        }
    }

    /// Pressing A on the obstacle offers the move.
    pub fn has_overworld_prompt(self) -> bool {
        !matches!(self, FieldMove::Fly | FieldMove::Flash | FieldMove::Dig)
    }
}

/// Whether `map` needs Flash to see (`"requires_flash": true` in the
/// decomp's `map.json`).
pub fn is_dark(world: &pokebot_world::World, map: &str) -> bool {
    world.map(map).is_some_and(|m| m.requires_flash)
}

/// Gates that come back when their map is loaded again.
pub fn regrows(kind: &str) -> bool {
    kind == "cut_tree" || kind == "rock_smash"
}

/// The party slot whose known moves include `mv`, from party knowledge
/// (lowest slot first).
pub fn carrier(state: &GameState, mv: &str) -> Option<u8> {
    state
        .party
        .value
        .as_ref()?
        .iter()
        .position(|m| {
            m.moves
                .iter()
                .flatten()
                .any(|s| s.mv.value.as_deref() == Some(mv))
        })
        .and_then(|slot| u8::try_from(slot).ok())
}

/// The YES row of a YES/NO menu as read, if the menu is one.
fn yes_no_rows(o: &Observation) -> Option<(u8, u8)> {
    let yes = o.menu_lines.iter().position(|l| fits("YES", l))?;
    let no = o.menu_lines.iter().position(|l| fits("NO", l))?;
    Some((u8::try_from(yes).ok()?, u8::try_from(no).ok()?))
}

/// Whether a page says the move was used.
fn says_used(page: &str, mv: FieldMove) -> bool {
    page.contains(&format!("used {}", mv.label()))
        || (mv == FieldMove::Strength && page.contains("possible to move"))
}

/// A text that ends the attempt without the move being used.
fn says_refused(page: &str) -> bool {
    // The font reads the game's apostrophe as ’.
    let page = page.replace('’', "'");
    [
        "Can't use that here",
        "can't be used here",
        "No SURFING here",
    ]
    .iter()
    .any(|t| page.contains(t))
}

/// The game's answer when the obstacle is already gone (`gText_NothingToCut`,
/// "There's nothing to CUT."): the way is clear, nothing is refused.
/// Also FLASH used with the light on: "This is in use already."
/// (`PARTY_MSG_ALREADY_IN_USE`; fleet continue-1, Rock Tunnel: the walk
/// lit the cave it started in, already lit, and the step waited on the
/// party menu until "no progress in after").
fn says_nothing_there(page: &str) -> bool {
    let page = page.replace('’', "'");
    page.contains("There's nothing to") || page.contains("in use already")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptPhase {
    Approach,
    Press,
    Talk,
}

/// Face the obstacle, press A, answer YES to the question naming the move,
/// read the pages to the end.
pub struct ObstaclePrompt {
    mv: FieldMove,
    phase: PromptPhase,
    go: GoStep,
    nav: NavParts,
    facing: Destination,
    retries: u32,
    /// The question naming the move was answered YES.
    pub offered: bool,
    /// A page said the move was used.
    pub used: bool,
    /// A page refused the move ("Can't use that here").
    pub refused: bool,
    /// Pages read, joined per page.
    pub pages: Vec<String>,
    /// Frames in a row the screen has been still after the last page, and
    /// the first quiet frame.
    still: u32,
    quiet_since: Option<u64>,
}

impl ObstaclePrompt {
    pub fn new(nav: &NavParts, mv: FieldMove, facing: Destination) -> Self {
        Self {
            mv,
            phase: PromptPhase::Approach,
            go: GoStep::with(nav, facing.clone()),
            nav: nav.clone(),
            facing,
            retries: 0,
            offered: false,
            used: false,
            refused: false,
            pages: Vec::new(),
            still: 0,
            quiet_since: None,
        }
    }

    /// Starts at the press (the player already faces the obstacle).
    pub fn facing_now(mut self) -> Self {
        self.phase = PromptPhase::Press;
        self
    }

    fn note_page(&mut self, o: &Observation) {
        let Some(d) = &o.dialogue else { return };
        let page = d.lines.join(" ");
        if page.is_empty() || self.pages.last() == Some(&page) {
            return;
        }
        // A page still printing is a prefix of the next reading.
        if let Some(last) = self.pages.last_mut() {
            if page.starts_with(last.as_str()) {
                *last = page.clone();
            } else {
                self.pages.push(page.clone());
            }
        } else {
            self.pages.push(page.clone());
        }
        self.used |= says_used(&page, self.mv);
        self.refused |= says_refused(&page);
    }

    /// The faced obstacle's tile matches the map's render (the object drawn
    /// over it is gone).
    fn obstacle_gone(
        &self,
        o: &Observation,
        frame: Option<&pokebot_core::NormalizedFrame>,
    ) -> bool {
        let (Some(frame), Some(p)) = (frame, &o.player) else {
            return false;
        };
        let Destination::Facing { map, x, y } = &self.facing else {
            return false;
        };
        if p.pose.map != *map {
            return false;
        }
        let Some(data) = self.nav.world.map(map) else {
            return false;
        };
        let Ok(render) = data.render() else {
            return false;
        };
        pokebot_world::localize::tile_score(frame.image(), render, data, &p.pose, (*x, *y))
            .is_some_and(|s| s >= OBSTACLE_GONE_SCORE)
    }

    fn talk(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        self.note_page(o);
        if let (Some(menu), Some(d)) = (&o.menu, &o.dialogue) {
            if let Some((yes, no)) = yes_no_rows(o) {
                // The YES/NO box shows before the question's last letters:
                // an A then only finishes the text.
                if d.stable_frames < QUESTION_PRINTED_FRAMES {
                    return Decision::Wait("the question is printing".into());
                }
                let question = o.dialogue.as_ref().map(|d| d.lines.join(" "));
                let names_move = question.is_some_and(|q| q.contains(self.mv.label()));
                let row = if names_move { yes } else { no };
                return crate::new_game::select(
                    menu,
                    row,
                    if names_move {
                        "YES: use the field move"
                    } else {
                        "NO: not our question"
                    },
                );
            }
        }
        if o.dialogue.is_some() {
            return crate::new_game::decline_or_advance(o, "reading");
        }
        if o.menu.is_some() {
            return Decision::Act(Action::new(
                "close an unexpected menu",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::MenuClosed,
                MENU_FRAMES,
            ));
        }
        if ctx.quiet_frames < PROMPT_SETTLE_FRAMES {
            return Decision::Wait("letting the field move play out".into());
        }
        // YES answered: the move's cut-in plays before "<mon> used <MOVE>"
        // prints (Switch, Victory Road: STRENGTH's page came 70-330 frames
        // after the YES; the step had given up after 50).
        if self.offered && !self.used && !self.refused && ctx.quiet_frames < USED_PAGE_FRAMES {
            return Decision::Wait("waiting for the move's page after YES".into());
        }
        if self.used {
            let since = *self.quiet_since.get_or_insert(o.frame_id);
            let waited = o.frame_id.saturating_sub(since);
            let settled = if matches!(self.mv, FieldMove::Cut | FieldMove::RockSmash) {
                // The tree or rock is gone once its tile looks like the
                // map's render again.
                self.obstacle_gone(o, ctx.frame)
            } else {
                self.still = if o.metrics.changed_pixels < STILL_PIXELS {
                    self.still + 1
                } else {
                    0
                };
                self.still >= STILL_FRAMES && o.player.is_some()
            };
            if !settled && waited < ANIMATION_MAX_FRAMES {
                return Decision::Wait("the field move's animation".into());
            }
        }
        if self.used {
            Decision::Done(format!("{} used", self.mv.label()))
        } else if self.refused {
            Decision::Fail(format!("{} can't be used here", self.mv.label()))
        } else if self.offered {
            Decision::Fail(format!(
                "answered YES to {} but no page said it was used",
                self.mv.label()
            ))
        } else {
            Decision::Fail(format!(
                "the game did not offer {} here (badge or move missing): {:?}",
                self.mv.label(),
                self.pages
            ))
        }
    }
}

impl ToolStep for ObstaclePrompt {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        match self.phase {
            PromptPhase::Approach => match self.go.next(ctx) {
                Decision::Done(_) => {
                    self.phase = PromptPhase::Press;
                    Decision::Wait("facing the obstacle".into())
                }
                d => d,
            },
            PromptPhase::Press => {
                if self.retries > MAX_PRESS_RETRIES {
                    return Decision::Fail(format!(
                        "{:?} does not answer to A: no {} prompt",
                        self.facing,
                        self.mv.label()
                    ));
                }
                Decision::Act(Action::new(
                    format!("press A on the obstacle ({})", self.mv.label()),
                    vec![ControllerCommand::Press(Button::A)],
                    Expectation::DialogueOpen,
                    45,
                ))
            }
            PromptPhase::Talk => self.talk(ctx),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        match self.phase {
            PromptPhase::Approach => self.go.on_outcome(action, outcome, ctx),
            PromptPhase::Press => match outcome {
                Outcome::Confirmed | Outcome::Interrupted => {
                    self.phase = PromptPhase::Talk;
                    self.note_page(ctx.observation);
                }
                _ => {
                    self.retries += 1;
                    self.phase = PromptPhase::Approach;
                    self.go = GoStep::with(&self.nav, self.facing.clone());
                }
            },
            PromptPhase::Talk => {
                if action.label.starts_with("YES") && outcome == Outcome::Confirmed {
                    self.offered = true;
                }
                self.note_page(ctx.observation);
            }
        }
    }

    fn expects(&self) -> Expects {
        match self.phase {
            PromptPhase::Approach => Expects::NONE,
            _ => Expects::DIALOGUE,
        }
    }
}

/// Start → POKéMON → `slot` → the move's row. Done once the move is
/// chosen and its screens are over: back in the overworld, or (Fly) the
/// region map is showing.
pub struct PartyFieldMove {
    mv: FieldMove,
    slot: u8,
    retries: Retries,
    closer: Closer,
    chosen: bool,
    /// Pages read after choosing the move.
    pub pages: Vec<String>,
    /// The Start menu may be opened without the player located (a dark
    /// cave before Flash).
    pub unlocated_ok: bool,
    failed: Option<String>,
    /// The game said there is nothing to use the move on (the obstacle is
    /// already gone).
    pub nothing_there: bool,
    /// Action windows closed because another member's opened.
    wrong_member: u32,
    /// The slot A was pressed on to open the action window: the window
    /// covers the bottom of the list, so the highlight of a member there
    /// can't be read under it (fleet continue-4: FLY on slot 5 failed "the
    /// action window never belonged to slot 5 (read None)").
    opened_on: Option<u8>,
}

impl PartyFieldMove {
    pub fn new(mv: FieldMove, slot: u8) -> Self {
        Self {
            mv,
            slot,
            retries: Retries::default(),
            closer: Closer::default(),
            chosen: false,
            pages: Vec::new(),
            unlocated_ok: false,
            failed: None,
            nothing_there: false,
            wrong_member: 0,
            opened_on: None,
        }
    }

    fn after_choice(&mut self, o: &Observation, quiet: u32) -> Decision {
        if self.mv == FieldMove::Fly && o.fly_map.is_some() {
            return Decision::Done("region map open".into());
        }
        // Dig asks first: "Want to escape from here and return to ROUTE
        // 4?" (`DisplayFieldMoveExitAreaMessage`).
        if self.mv == FieldMove::Dig {
            if let (Some(menu), Some((yes, _))) = (&o.menu, yes_no_rows(o)) {
                return crate::new_game::select(menu, yes, "YES: dig out");
            }
        }
        if let Some(d) = &o.dialogue {
            let page = d.lines.join(" ");
            if !page.is_empty() && self.pages.last() != Some(&page) {
                if says_nothing_there(&page) {
                    self.nothing_there = true;
                } else if says_refused(&page) || page.contains("not able") {
                    self.failed = Some(page.clone());
                }
                self.pages.push(page);
            }
            return crate::new_game::advance_or_wait(Some(d), "reading");
        }
        if let Some(why) = &self.failed {
            // Close the party menu the refusal leaves open.
            if o.party_menu.is_some() || o.menu.is_some() {
                return self.closer.next(o, "closed");
            }
            return Decision::Fail(format!("{}: {why}", self.mv.label()));
        }
        if let Some(party) = &o.party_menu {
            // Refusals print in the party menu's prompt box; so does
            // "There's nothing to CUT." (Switch, Vermilion: the tree was
            // already cut, the list stayed open and the walk to the Gym
            // failed "no progress in after").
            if says_nothing_there(&party.prompt) {
                self.nothing_there = true;
            } else if says_refused(&party.prompt) {
                self.failed = Some(party.prompt.clone());
                return Decision::Wait("refused".into());
            }
        }
        if self.nothing_there {
            if o.party_menu.is_some() || o.menu.is_some() {
                return self.closer.next(o, "closed");
            }
            return Decision::Done(format!(
                "nothing to {} here: the way is clear",
                self.mv.label()
            ));
        }
        if o.party_menu.is_some() || o.summary.is_some() || o.bag.is_some() {
            return self.retries.wait(o, "waiting for the party menu to close");
        }
        if o.player.is_none() && !self.unlocated_ok {
            return self.retries.wait(o, "waiting for the field move to finish");
        }
        if quiet < PROMPT_SETTLE_FRAMES {
            return Decision::Wait("letting the field move play out".into());
        }
        Decision::Done(format!("{} used from the party menu", self.mv.label()))
    }
}

impl ToolStep for PartyFieldMove {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn on_outcome(&mut self, _: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.retries.exhausted() {
            return Decision::Fail(format!(
                "{} from the party menu: no progress in {}",
                self.mv.label(),
                self.retries.phase()
            ));
        }
        if self.chosen {
            self.retries.enter("after");
            return self.after_choice(o, ctx.quiet_frames);
        }
        if let Some(party) = &o.party_menu {
            if !party.options.is_empty() {
                self.retries.enter("actions");
                if party.selected.or(self.opened_on) != Some(self.slot) {
                    // The window of another member: back to the list.
                    self.wrong_member += 1;
                    if self.wrong_member > 3 {
                        return Decision::Fail(format!(
                            "the action window never belonged to slot {} (read {:?})",
                            self.slot, party.selected
                        ));
                    }
                    self.opened_on = None;
                    return self.retries.act(
                        "close the wrong member's actions",
                        Button::B,
                        Expectation::PartyList,
                        SCREEN_FRAMES,
                    );
                }
                let Some(row) = party.options.iter().position(|l| fits(self.mv.label(), l)) else {
                    self.failed = Some(format!(
                        "slot {} has no {} in its actions {:?}",
                        self.slot,
                        self.mv.label(),
                        party.options
                    ));
                    self.chosen = true;
                    return self.retries.wait(o, "closing");
                };
                let Some(at) = party.option_cursor else {
                    return self.retries.wait(o, "reading the action window's ▶");
                };
                let row = row as u8;
                if at == row {
                    self.chosen = true;
                    return self.retries.act(
                        format!("choose {}", self.mv.label()),
                        Button::A,
                        Expectation::InputsDone,
                        SCREEN_FRAMES,
                    );
                }
                let (button, next) = if at < row {
                    (Button::Down, at + 1)
                } else {
                    (Button::Up, at - 1)
                };
                return self.retries.act(
                    format!("actions: {button:?} toward {}", self.mv.label()),
                    button,
                    Expectation::PartyOptionAt(next),
                    CURSOR_FRAMES,
                );
            }
            self.retries.enter("party");
            if self.slot >= party.count {
                return Decision::Fail(format!(
                    "slot {} is not in the party ({} members)",
                    self.slot, party.count
                ));
            }
            let Some(at) = party.selected else {
                return self.retries.wait(o, "reading the selected member");
            };
            if at == self.slot {
                self.opened_on = Some(at);
                return self.retries.act(
                    "open the member's actions",
                    Button::A,
                    Expectation::PartyActions,
                    SCREEN_FRAMES,
                );
            }
            let (button, next) = if at < self.slot {
                (Button::Down, at + 1)
            } else {
                (Button::Up, at - 1)
            };
            return self.retries.act(
                format!("party: {button:?} toward slot {}", self.slot),
                button,
                Expectation::PartySelected(next),
                CURSOR_FRAMES,
            );
        }
        if let Some(menu) = &o.menu {
            if o.dialogue.is_none() {
                self.retries.enter("start menu");
                return pick_row(
                    &mut self.retries,
                    o,
                    menu,
                    &MenuRow::Text("POKéMON"),
                    ctx.state,
                    Expectation::PartyList,
                    SCREEN_FRAMES,
                )
                .0;
            }
        }
        if o.dialogue.is_some() {
            return crate::new_game::decline_or_advance(o, "reading");
        }
        // Another screen left up (the bag a medicine came back to): Start
        // does nothing there; B backs out of it (fleet continue-5,
        // Victory Road 3F: a HYPER POTION used, the bag still up, then
        // "STRENGTH from the party menu: no progress in open").
        if o.bag.is_some() || o.summary.is_some() || o.trainer_card.is_some() {
            self.retries.enter("back out");
            return self.retries.act(
                "back out of the screen left up",
                Button::B,
                Expectation::InputsDone,
                SCREEN_FRAMES,
            );
        }
        self.retries.enter("open");
        if self.unlocated_ok && o.player.is_none() && ctx.quiet_frames >= 30 {
            return self.retries.act(
                "open the Start menu (dark)",
                Button::Start,
                Expectation::MenuOpen,
                MENU_FRAMES,
            );
        }
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}

/// Frames the fly icons are watched for before a destination counts as
/// not visited (they blink: two thirds of the Switch's frames drew none).
const FLY_ICON_WAIT_FRAMES: u64 = 120;

/// On the region map: walk the cursor to `cell`, press A, and wait until
/// the player is located on `map`.
pub struct FlyTo {
    map: String,
    cell: (u32, u32),
    retries: Retries,
    pressed: Option<u64>,
    /// The destination's fly icon was seen: the icons blink (on the Switch
    /// drawn one frame in three), so one frame without it proves nothing.
    lit_seen: bool,
    /// The first frame the region map was seen.
    map_since: Option<u64>,
    /// Where the flight lands (`places.json` fly spots).
    landing: Option<pokebot_state::PlayerPose>,
}

impl FlyTo {
    pub fn new(map: &str) -> Option<Self> {
        Some(Self {
            map: map.to_owned(),
            cell: fly_map::spot_cell(map)?,
            retries: Retries::default(),
            pressed: None,
            lit_seen: false,
            map_since: None,
            landing: None,
        })
    }

    /// Where the flight lands: the player is placed there once the
    /// destination's name shows (the tracker, still on the old map, found
    /// the player nowhere: the Switch flew to Lavender and waited "flying"
    /// until the step failed).
    pub fn landing_at(mut self, pose: pokebot_state::PlayerPose) -> Self {
        self.landing = Some(pose);
        self
    }

    /// Whether the map-name popup names this destination ("LAVENDER TOWN"
    /// for LavenderTown, "INDIGO PLATEAU" for IndigoPlateau_Exterior).
    fn names_destination(&self, popup: &str) -> bool {
        let key = |s: &str| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_uppercase()
        };
        let popup = key(popup);
        !popup.is_empty() && key(&self.map).starts_with(&popup)
    }

    /// The next cursor cell one press brings toward the target: columns
    /// first, then rows.
    pub fn toward(from: (u32, u32), to: (u32, u32)) -> Option<(Button, (u32, u32))> {
        use std::cmp::Ordering::*;
        match (from.0.cmp(&to.0), from.1.cmp(&to.1)) {
            (Less, _) => Some((Button::Right, (from.0 + 1, from.1))),
            (Greater, _) => Some((Button::Left, (from.0 - 1, from.1))),
            (Equal, Less) => Some((Button::Down, (from.0, from.1 + 1))),
            (Equal, Greater) => Some((Button::Up, (from.0, from.1 - 1))),
            (Equal, Equal) => None,
        }
    }
}

impl ToolStep for FlyTo {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn on_outcome(&mut self, _: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.retries.exhausted() {
            return Decision::Fail(format!("fly to {}: no progress", self.map));
        }
        if let Some(p) = &o.player {
            if self.pressed.is_some() && p.pose.map == self.map {
                return Decision::Done(format!("flew to {}", self.map));
            }
        }
        if let (Some(_), Some(landing), Some(popup)) =
            (self.pressed, self.landing.as_ref(), o.map_popup.as_deref())
        {
            if o.fly_map.is_none() && self.names_destination(popup) {
                ctx.events.push(GameEvent::PlayerInferred {
                    pose: landing.clone(),
                    candidates: Vec::new(),
                });
                return Decision::Done(format!("flew to {} ({popup})", self.map));
            }
        }
        let Some(map) = &o.fly_map else {
            if self.pressed.is_some() {
                return Decision::Wait("flying".into());
            }
            return self.retries.wait(o, "waiting for the region map");
        };
        let since = *self.map_since.get_or_insert(o.frame_id);
        self.lit_seen |= map.lit.iter().any(|m| m == &self.map);
        if !self.lit_seen {
            if o.frame_id.saturating_sub(since) < FLY_ICON_WAIT_FRAMES {
                return Decision::Wait("waiting for the fly icons (they blink)".into());
            }
            return Decision::Fail(format!(
                "{} is not lit on the fly map (not visited)",
                self.map
            ));
        }
        if let Some(at) = self.pressed {
            if o.frame_id.saturating_sub(at) < FLY_TAKEOFF_FRAMES {
                return Decision::Wait("taking off".into());
            }
            // Still on the map: the press didn't take.
            self.pressed = None;
            self.retries.failed();
        }
        let Some(cursor) = map.cursor else {
            return self.retries.wait(o, "looking for the map cursor");
        };
        self.retries.enter("cursor");
        match FlyTo::toward(cursor, self.cell) {
            None => {
                self.pressed = Some(o.frame_id);
                self.retries.act(
                    format!("fly to {}", self.map),
                    Button::A,
                    Expectation::InputsDone,
                    SCREEN_FRAMES,
                )
            }
            Some((button, next)) => self.retries.act(
                format!("map cursor {button:?} toward {}", self.map),
                button,
                Expectation::FlyCursorAt(next.0, next.1),
                CURSOR_FRAMES,
            ),
        }
    }
}

/// One Strength push: tap toward the boulder from behind it; the player
/// steps into the boulder's old tile.
struct Push {
    dir: Direction,
    from: pokebot_state::PlayerPose,
    /// The boulder's tile before the push.
    boulder: (i32, i32),
    world: std::sync::Arc<pokebot_world::World>,
    done: bool,
    tries: u32,
    /// The tap was sent: the boulder is judged once the screen settles.
    tapped: bool,
    /// Battles begun by the time of the tap.
    battles: usize,
    /// The frame of the tap.
    tapped_at: u64,
}

/// Frames a pushed boulder takes to stop: it slides at half walking speed
/// (WALK_SLOWER, 32 frames), and the dust after it.
const PUSH_SLIDE_FRAMES: u64 = 60;

/// The battles in `learned`: one begun after a tap ate it.
fn battles_in(learned: &[GameEvent]) -> usize {
    learned
        .iter()
        .filter(|e| matches!(e, GameEvent::BattleStarted))
        .count()
}

/// Whether the screen shows the boulder pushed from `old` one tile `dir`:
/// its sprite on the new tile, or the old tile bare floor again and the
/// new one not (or, a hole there, the boulder fallen through). The player
/// walks in place while the boulder moves (`DoBoulderDust`: the player's
/// WALK_IN_PLACE, the boulder's WALK_SLOWER). With nothing to tell by, the
/// tap is taken to have pushed: a second tap would push it again.
fn pushed(
    world: &pokebot_world::World,
    o: &Observation,
    frame: Option<&pokebot_core::NormalizedFrame>,
    old: (i32, i32),
    dir: Direction,
) -> bool {
    let (dx, dy) = dir.delta();
    let new = (old.0 + dx, old.1 + dy);
    if let Some(seen) = boulder_moved(o, old, new) {
        return seen;
    }
    let falls = o
        .player
        .as_ref()
        .and_then(|p| world.map(&p.pose.map))
        .and_then(|m| m.tile(new.0, new.1))
        .is_some_and(|t| t.behavior == pokebot_world::boulders::FALL_WARP);
    // The top halves: a boulder is drawn half a tile up, over the tile
    // above's bottom half, and the player's head over the tile above it
    // (fleet continue-2, Victory Road 2F: pushed Down from (6, 17), the
    // boulder at rest on (6, 18) covered (6, 17)'s bottom half, which never
    // read bare; the push was judged failed three times).
    match (
        bare_top(world, o, frame, old),
        bare_top(world, o, frame, new),
    ) {
        (Some(true), _) if falls => true,
        (Some(true), Some(false)) => true,
        (Some(false), _) | (_, Some(true)) => false,
        _ => true,
    }
}

/// Whether the top half of tile `at` looks like the map's floor there:
/// what stands on a tile is drawn over its top half, what stands below it
/// only over its bottom half.
fn bare_top(
    world: &pokebot_world::World,
    o: &Observation,
    frame: Option<&pokebot_core::NormalizedFrame>,
    at: (i32, i32),
) -> Option<bool> {
    let (frame, p) = (frame?, o.player.as_ref()?);
    let data = world.map(&p.pose.map)?;
    let render = data.render().ok()?;
    pokebot_world::localize::tile_rows_score(
        frame.image(),
        render,
        data,
        &p.pose,
        at,
        0..pokebot_world::BLOCK / 2,
    )
    .map(|s| s >= OBSTACLE_GONE_SCORE)
}

/// Whether the screen shows the boulder pushed from `old` to `new`: a
/// sprite on its new tile and none left on the old one. `None` when no
/// sprite is read near either (the move is taken on the player's word).
fn boulder_moved(o: &Observation, old: (i32, i32), new: (i32, i32)) -> Option<bool> {
    let at = |t: (i32, i32)| o.sprites.iter().any(|s| (s.x, s.y) == t);
    match (at(old), at(new)) {
        (true, _) => Some(false),
        (false, true) => Some(true),
        (false, false) => None,
    }
}

impl ToolStep for Push {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        if self.done {
            return Decision::Done("pushed".into());
        }
        if self.tapped {
            // A wild battle after the tap: the tap is lost to it, tried again
            // without counting (fleet continue-1, Victory Road 1F: battles
            // between taps spent the tries, the boulder never pushed).
            if battles_in(ctx.learned) > self.battles {
                self.tapped = false;
                return Decision::Wait("a battle came between: pushing again".into());
            }
            // The boulder slides for a while after the tap: judged once it
            // has stopped (fleet continue-1, Victory Road 1F: judged mid
            // slide, the boulder half on the tile it left, every push Right
            // "did not move").
            let since = ctx.observation.frame_id.saturating_sub(self.tapped_at);
            if ctx.quiet_frames < 20 || since < PUSH_SLIDE_FRAMES {
                return Decision::Wait("the boulder moving".into());
            }
            self.tapped = false;
            // A push never moves the player: a step is a walk, not a push.
            let walked = ctx
                .observation
                .player
                .as_ref()
                .is_some_and(|p| p.pose != self.from);
            if !walked
                && pushed(
                    &self.world,
                    ctx.observation,
                    ctx.frame,
                    self.boulder,
                    self.dir,
                )
            {
                self.done = true;
                return Decision::Done("pushed".into());
            }
            self.tries += 1;
        }
        if self.tries >= 3 {
            return Decision::Fail(format!("the boulder did not move {:?}", self.dir));
        }
        if ctx.quiet_frames < 20 {
            return Decision::Wait("settling before the push".into());
        }
        // One tap: a held direction pushed and then walked on, a second
        // push or a step astray (Switch, Victory Road 1F: 400 ms held, the
        // player went on into the boulder's new tile 15 frames after the
        // push). A tap that only turns the player toward the boulder makes
        // no move: the next try pushes. The player stays where it is
        // (fleet continue-3/6, Victory Road: a push judged by the player's
        // step never was one, the next tap pushed again or walked into the
        // gap, and the boulder ended up two tiles off).
        Decision::Act(
            Action::new(
                format!("push the boulder {:?}", self.dir),
                vec![ControllerCommand::Press(direction_button(self.dir))],
                Expectation::InputsDone,
                90,
            )
            .timed(crate::motion::InputKind::WalkTile, 1),
        )
    }

    fn on_outcome(&mut self, _: &Action, _: Outcome, ctx: &mut StepContext<'_>) {
        self.tapped = true;
        self.battles = battles_in(ctx.learned);
        self.tapped_at = ctx.observation.frame_id;
    }
}

/// Uses `mv` on the obstacle at `at` (overworld prompt; the party-menu
/// path when the move has none or A offered nothing).
pub fn use_on_obstacle(
    ctx: &mut ToolContext<'_>,
    mv: FieldMove,
    at: &Dest,
) -> Result<(), ToolError> {
    let facing = Destination::from(at);
    let nav = NavParts::of(ctx);
    let mut prompt = ObstaclePrompt::new(&nav, mv, facing.clone());
    let result = ctx.drive(&mut prompt);
    match result {
        Ok(_) => {
            // A cut tree or smashed rock is gone until its map loads again
            // (`ToolContext` forgets it on the next entry).
            if let Dest::Facing { map, x, y } = at {
                if let Some(gate) = ctx.world.places().and_then(|p| {
                    p.gates
                        .iter()
                        .find(|g| g.map == *map && (g.x, g.y) == (*x, *y) && regrows(&g.kind))
                }) {
                    ctx.gone.insert((gate.map.clone(), gate.local_id));
                }
            }
            ctx.emit(progress(
                "FieldMove",
                format!("{} used at {at:?}", mv.label()),
            ))?;
            Ok(())
        }
        Err(ToolError::Failed(why)) if !prompt.offered && !prompt.refused => {
            // The game offered nothing on A: the party-menu path, still
            // facing the obstacle.
            let slot = carrier(ctx.state(), mv.move_id()).ok_or_else(|| {
                ToolError::Failed(format!(
                    "{why}; no party member is known to know {}",
                    mv.label()
                ))
            })?;
            ctx.info(format!(
                "field move: {why}; trying {} from the party menu",
                mv.label()
            ));
            let mut step = PartyFieldMove::new(mv, slot);
            ctx.drive(&mut step)?;
            if step.nothing_there {
                // Gone already: the walk goes through where it stood.
                if let Dest::Facing { map, x, y } = at {
                    if let Some(gate) = ctx.world.places().and_then(|p| {
                        p.gates
                            .iter()
                            .find(|g| g.map == *map && (g.x, g.y) == (*x, *y) && regrows(&g.kind))
                    }) {
                        ctx.gone.insert((gate.map.clone(), gate.local_id));
                    }
                }
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Whether `mv` aimed at `at` is used from the party menu, facing where
/// the player faces: FLASH always, and SURF with no tile named, over a
/// map edge (fleet continue-5, Cinnabar's north shore: Route 21's water is
/// across the edge, where no tile of the shore's map lies, and every
/// SURF failed "needs the obstacle's tile").
fn from_party(mv: FieldMove, at: Option<&Dest>) -> bool {
    match mv {
        FieldMove::Flash => true,
        FieldMove::Surf => at.is_none(),
        _ => false,
    }
}

/// Uses `mv` from the party menu (Flash; the first half of Fly).
pub fn use_from_party(ctx: &mut ToolContext<'_>, mv: FieldMove) -> Result<(), ToolError> {
    let slot = carrier(ctx.state(), mv.move_id()).ok_or_else(|| {
        ToolError::Failed(format!("no party member is known to know {}", mv.label()))
    })?;
    let mut step = PartyFieldMove::new(mv, slot);
    step.unlocated_ok = mv == FieldMove::Flash;
    ctx.drive(&mut step)?;
    Ok(())
}

/// Closes what is open, back to the overworld.
struct CloseScreens(super::menu::Closer);

impl ToolStep for CloseScreens {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        self.0.next(ctx.observation, "closed")
    }
}

/// Flies to `map` (a lit fly spot).
pub fn fly(ctx: &mut ToolContext<'_>, map: &str) -> Result<(), ToolError> {
    let mut to = FlyTo::new(map)
        .ok_or_else(|| ToolError::Failed(format!("{map} is not a fly spot on the region map")))?;
    let spot = ctx
        .world
        .places()
        .and_then(|p| p.fly_spots.iter().find(|f| f.map == map))
        .cloned();
    if let Some(spot) = &spot {
        to = to.landing_at(pokebot_state::PlayerPose {
            map: spot.map.clone(),
            x: spot.x,
            y: spot.y,
        });
    }
    use_from_party(ctx, FieldMove::Fly)?;
    match ctx.drive(&mut to) {
        // Not lit, watched past its blinking: the spot's flag is clear (a
        // walk through Route 10 is no visit of its Center, which lights
        // it). The map is closed, or the next try opens on it (fleet
        // continue-5: "FLY from the party menu: no progress in open",
        // every replan).
        Err(ToolError::Failed(e)) if e.contains("is not lit on the fly map") => {
            if let Some(spot) = &spot {
                ctx.emit(GameEvent::FlagObserved {
                    flag: spot.flag.clone(),
                    value: false,
                })?;
            }
            let _ = ctx.drive(&mut CloseScreens(super::menu::Closer::to(
                super::menu::Leave::Overworld,
            )));
            return Err(ToolError::Failed(e));
        }
        r => r?,
    };
    ctx.emit(GameEvent::MapVisited {
        map: map.to_owned(),
    })?;
    Ok(())
}

/// `nav` with the boulder at `tile` of `map` a closed tile: the walk
/// goes round it, never into it.
fn boulder_walled(mut nav: NavParts, map: &str, tile: (i32, i32)) -> NavParts {
    let mut gates = (*nav.gates).clone();
    gates.closed.entry(map.to_owned()).or_default().insert(tile);
    nav.gates = std::sync::Arc::new(gates);
    nav
}

/// [`boulder_walled`], the boulder's map object set aside: it isn't on the
/// tile it spawned on once pushed (fleet continue-1, Victory Road 3F: the
/// walk to the next push needed the boulder's first tile, (32, 5), and
/// failed "no path to (32, 5)", the object still held there).
fn pushed_boulder_walled(
    nav: NavParts,
    map: &str,
    spawn: (i32, i32),
    tile: (i32, i32),
) -> NavParts {
    let mut nav = boulder_walled(nav, map, tile);
    let id = nav.world.map(map).and_then(|m| {
        m.objects
            .iter()
            .find(|o| {
                pokebot_world::boulders::is_boulder(o)
                    && (o.x, o.y) == (Some(spawn.0), Some(spawn.1))
            })
            .map(|o| o.local_id)
    });
    if let Some(id) = id {
        nav.gone.insert((map.to_owned(), id));
    }
    nav
}

/// Pushes that take one of `boulders` onto `target` on `map` from the
/// player at `player`: the boulder, and its pushes in order
/// ([`pokebot_world::boulders::pushes`]).
pub fn boulder_pushes(
    map: &pokebot_world::MapData,
    boulders: &[(i32, i32)],
    blocked: &pokebot_world::path::Obstacles,
    player: (i32, i32),
    target: (i32, i32),
) -> Option<((i32, i32), Vec<Direction>)> {
    pokebot_world::boulders::pushes(map, boulders, blocked, player, &[target])
}

/// Activates Strength on the boulder at `(x, y)` of `map`, then pushes it
/// along `pushes`, walking behind it before each push.
/// What a boulder sequence fails with when the map was left part way: on
/// re-entry the boulders are back on their own tiles and STRENGTH is off
/// (`ClearTempFieldEventData`), so the pushes start over.
pub const LEFT_MID_PUSH: &str = "left the map mid-push";

/// Fails with [`LEFT_MID_PUSH`] when the map was entered again since
/// visit `visit` (now `now`).
fn left_mid_push(visit: u64, now: u64) -> Result<(), ToolError> {
    if now == visit {
        return Ok(());
    }
    Err(ToolError::Failed(format!(
        "{LEFT_MID_PUSH}: the boulders are back and STRENGTH is off"
    )))
}

pub fn strength(
    ctx: &mut ToolContext<'_>,
    map: &str,
    (x, y): (i32, i32),
    pushes: &[Direction],
) -> Result<(), ToolError> {
    let at = Dest::Facing {
        map: map.to_owned(),
        x,
        y,
    };
    use_on_obstacle(ctx, FieldMove::Strength, &at)?;
    let visit = ctx.map_visits;
    // A heal, a white-out or a reroute between pushes leaves the map
    // (fleet continue-5, Victory Road 1F: flown to Viridian to heal after
    // a wild battle, back, and the pushes went on from where the boulder
    // had been, STRENGTH off: "the boulder did not move Down").
    let left = |ctx: &ToolContext<'_>| left_mid_push(visit, ctx.map_visits);
    let mut boulder = (x, y);
    for &dir in pushes {
        let (dx, dy) = dir.delta();
        let behind = (boulder.0 - dx, boulder.1 - dy);
        // The boulder's tile is a wall to the walk behind it: with Strength
        // on, a step into it pushes it (Switch, Victory Road 1F: the walk
        // round to the next push went through the boulder's tile, pushed
        // it off the planned line, and the pushes after met nothing).
        let nav = pushed_boulder_walled(NavParts::of(ctx), map, (x, y), boulder);
        let mut walk = GoStep::with(
            &nav,
            Destination::Tile {
                map: map.to_owned(),
                x: behind.0,
                y: behind.1,
            },
        );
        ctx.drive(&mut walk)?;
        left(ctx)?;
        let from = ctx
            .pose()
            .ok_or_else(|| ToolError::Failed("not located behind the boulder".into()))?;
        let mut push = Push {
            dir,
            from,
            boulder,
            world: std::sync::Arc::clone(&ctx.world),
            done: false,
            tries: 0,
            tapped: false,
            battles: 0,
            tapped_at: 0,
        };
        ctx.drive(&mut push)?;
        left(ctx)?;
        // The boulder's old tile is free, its new one blocks.
        ctx.blocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(map, boulder);
        boulder = (boulder.0 + dx, boulder.1 + dy);
        ctx.blocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(map, boulder);
        ctx.emit(GameEvent::TileBlocked {
            map: map.to_owned(),
            x: boulder.0,
            y: boulder.1,
        })?;
    }
    Ok(())
}

/// `FieldMove { mv, at, push }`.
pub struct FieldMoveTool;

impl Tool for FieldMoveTool {
    fn name(&self) -> &str {
        "FieldMove"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::FieldMove { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::FieldMove { mv, at, push } = intent else {
            return ToolOutcome::failed("not a FieldMove");
        };
        let Some(field) = FieldMove::from_move(mv) else {
            return ToolOutcome::failed(format!("{mv} is not a field move"));
        };
        let result = match (field, at) {
            (FieldMove::Fly, Some(dest)) => fly(ctx, dest.map()),
            (FieldMove::Fly, None) => Err(ToolError::Failed("Fly needs a destination".into())),
            (_, at) if from_party(field, at.as_ref()) => use_from_party(ctx, field),
            (FieldMove::Dig, _) => super::escape::dig(ctx),
            (FieldMove::Strength, Some(Dest::Facing { map, x, y })) => {
                strength(ctx, map, (*x, *y), push)
            }
            (_, Some(dest @ Dest::Facing { .. })) => use_on_obstacle(ctx, field, dest),
            (_, _) => Err(ToolError::Failed(format!(
                "{} needs the obstacle's tile (Dest::Facing)",
                field.label()
            ))),
        };
        result.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fleet continue-5 on Cinnabar's north shore: the route surfs onto
    /// Route 21's water across the map edge, SURF named no tile, and the
    /// tool failed "needs the obstacle's tile" every plan. SURF with no
    /// tile is used from the party menu, facing the water; with one, on it.
    #[test]
    fn surf_with_no_tile_is_used_from_the_party_menu() {
        let shore = Dest::Facing {
            map: "Route21_South".into(),
            x: 14,
            y: 49,
        };
        assert!(from_party(FieldMove::Surf, None));
        assert!(!from_party(FieldMove::Surf, Some(&shore)));
        assert!(from_party(FieldMove::Flash, None));
        assert!(!from_party(FieldMove::Cut, None));
    }
    use pokebot_state::{
        DialogueKind, DialogueObservation, FlyMapObservation, Knowledge, MenuObservation, MoveSlot,
        Observed, PartyMenuObservation, PartyMon, Region, ScreenState,
    };

    fn bare(frame: u64) -> Observation {
        Observation::bare(
            frame,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        )
    }

    fn dialogue(lines: &[&str], waiting: bool) -> DialogueObservation {
        DialogueObservation {
            kind: DialogueKind::MessageBox,
            region: Region::new(8, 118, 224, 36),
            waiting_for_input: waiting,
            arrow: None,
            stable_frames: if waiting { 60 } else { 0 },
            text_cells: vec![1; 8],
            lines: lines.iter().map(|s| (*s).to_owned()).collect(),
            help: false,
        }
    }

    fn yes_no(cursor: u8) -> MenuObservation {
        MenuObservation {
            window: Region::new(166, 70, 50, 32),
            rows: 2,
            cursor_row: cursor,
            cursor_y: 76 + 16 * u32::from(cursor),
        }
    }

    fn step_ctx<'a>(
        o: &'a Observation,
        state: &'a GameState,
        events: &'a mut Vec<GameEvent>,
        quiet: u32,
    ) -> StepContext<'a> {
        StepContext {
            observation: o,
            state,
            events,
            quiet_frames: quiet,
            frame: None,
            learned: &[],
        }
    }

    fn world() -> Option<std::sync::Arc<pokebot_world::World>> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        pokebot_world::World::load(&dir)
            .ok()
            .map(std::sync::Arc::new)
    }

    fn pressed(d: &Decision) -> Option<Button> {
        match d {
            Decision::Act(a) => match a.commands.first() {
                Some(ControllerCommand::Press(b)) => Some(*b),
                _ => None,
            },
            _ => None,
        }
    }

    /// The cut tree's pages as recorded on the emulator: the question,
    /// the YES/NO with the ▶ on YES, "IVYSAUR used CUT!", then quiet.
    #[test]
    fn the_cut_prompt_is_answered_yes_and_verified_by_its_text() {
        let Some(world) = world() else { return };
        let nav = NavParts {
            story: None,
            world,
            gone: Default::default(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
            data: None,
        };
        let facing = Destination::Facing {
            map: "CeruleanCity".into(),
            x: 26,
            y: 32,
        };
        let mut step = ObstaclePrompt::new(&nav, FieldMove::Cut, facing).facing_now();
        let state = GameState::default();
        let mut events = Vec::new();
        let o = bare(1);
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 200));
        assert_eq!(pressed(&d), Some(Button::A));
        let Decision::Act(a) = d else { unreachable!() };
        let mut o = bare(2);
        o.dialogue = Some(dialogue(
            &["This tree looks like it can be CUT", "down!"],
            true,
        ));
        step.on_outcome(
            &a,
            Outcome::Confirmed,
            &mut step_ctx(&o, &state, &mut events, 0),
        );
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A), "advance the first page");
        // The question with YES/NO, ▶ on YES: YES is chosen.
        let mut o = bare(3);
        o.dialogue = Some(dialogue(&["Would you like to CUT it"], false));
        o.menu = Some(yes_no(0));
        o.menu_lines = vec!["YES".into(), "NO".into()];
        // The box shows before the "?" is printed (as recorded): wait.
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Wait(_)
        ));
        let mut printed = dialogue(&["Would you like to CUT it?"], false);
        printed.stable_frames = 10;
        o.dialogue = Some(printed);
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        let Decision::Act(a) = d else { unreachable!() };
        assert!(a.label.starts_with("YES"));
        step.on_outcome(
            &a,
            Outcome::Confirmed,
            &mut step_ctx(&o, &state, &mut events, 0),
        );
        assert!(step.offered);
        // ▶ on NO: moved up to YES first.
        o.menu = Some(yes_no(1));
        assert_eq!(
            pressed(&step.next(&mut step_ctx(&o, &state, &mut events, 0))),
            Some(Button::Up)
        );
        let mut o = bare(4);
        o.dialogue = Some(dialogue(&["IVYSAUR used CUT!"], true));
        let _ = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert!(step.used);
        // The tree's animation is waited out until its tile looks like the
        // map's render (recorded frames: the tree still there under the
        // question, then gone).
        let o = bare(100);
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 10)),
            Decision::Wait(_)
        ));
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let load = |name: &str| {
            let image = pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()?;
            pokebot_core::NormalizedFrame::new(0, std::time::Instant::now(), image).ok()
        };
        let (Some(tree), Some(gone)) =
            (load("emu-cut-question.png"), load("emu-cut-tree-gone.png"))
        else {
            return;
        };
        let mut o = bare(120);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "CeruleanCity".into(),
                x: 26,
                y: 31,
            },
            score: 1000,
        });
        let mut ctx = step_ctx(&o, &state, &mut events, 80);
        ctx.frame = Some(&tree);
        assert!(
            matches!(step.next(&mut ctx), Decision::Wait(_)),
            "the tree is there"
        );
        o.frame_id = 130;
        let mut ctx = step_ctx(&o, &state, &mut events, 90);
        ctx.frame = Some(&gone);
        assert!(
            matches!(step.next(&mut ctx), Decision::Done(_)),
            "the tree is gone"
        );
    }

    /// Without the badge or the move the tree only says it can be cut: the
    /// prompt fails without having been offered (the caller may try the
    /// party menu, or replan).
    /// Switch, Victory Road: YES to STRENGTH, then the move's cut-in plays
    /// with no dialogue before "LAPRAS used STRENGTH!" prints; the step
    /// gave up after 50 quiet frames. It waits for the page, and fails
    /// only long after.
    #[test]
    fn the_moves_page_is_waited_for_after_yes() {
        let Some(world) = world() else { return };
        let nav = NavParts {
            story: None,
            world,
            gone: Default::default(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
            data: None,
        };
        let facing = Destination::Facing {
            map: "VictoryRoad_1F".into(),
            x: 7,
            y: 18,
        };
        let mut step = ObstaclePrompt::new(&nav, FieldMove::Strength, facing).facing_now();
        // Past the question: YES answered, the dialogue closed.
        step.phase = PromptPhase::Talk;
        step.offered = true;
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(100);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "VictoryRoad_1F".into(),
                x: 7,
                y: 17,
            },
            score: 1000,
        });
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 60)),
            Decision::Wait(_)
        ));
        match step.next(&mut step_ctx(&o, &state, &mut events, 600)) {
            Decision::Fail(why) => assert!(why.contains("no page said it was used"), "{why}"),
            _ => panic!("expected the failure, long after"),
        }
    }

    #[test]
    fn a_tree_that_is_not_offered_fails_the_prompt() {
        let Some(world) = world() else { return };
        let nav = NavParts {
            story: None,
            world,
            gone: Default::default(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
            data: None,
        };
        let facing = Destination::Facing {
            map: "CeruleanCity".into(),
            x: 26,
            y: 32,
        };
        let mut step = ObstaclePrompt::new(&nav, FieldMove::Cut, facing).facing_now();
        let state = GameState::default();
        let mut events = Vec::new();
        let Decision::Act(a) = step.next(&mut step_ctx(&bare(1), &state, &mut events, 200)) else {
            panic!("press A")
        };
        let mut o = bare(2);
        o.dialogue = Some(dialogue(
            &["This tree looks like it can be CUT", "down!"],
            true,
        ));
        step.on_outcome(
            &a,
            Outcome::Confirmed,
            &mut step_ctx(&o, &state, &mut events, 0),
        );
        let o = bare(50);
        match step.next(&mut step_ctx(&o, &state, &mut events, 60)) {
            Decision::Fail(why) => assert!(why.contains("did not offer CUT"), "{why}"),
            _ => panic!("expected a failure"),
        }
        assert!(!step.offered && !step.used);
    }

    fn party_menu(selected: u8, options: &[&str], cursor: Option<u8>) -> PartyMenuObservation {
        PartyMenuObservation {
            count: 5,
            selected: Some(selected),
            actions: !options.is_empty(),
            prompt: if options.is_empty() {
                "Choose a POKéMON.".into()
            } else {
                String::new()
            },
            options: options.iter().map(|s| (*s).to_owned()).collect(),
            option_cursor: cursor,
            able: Vec::new(),
            members: Vec::new(),
            double: false,
        }
    }

    /// Fleet continue-5: a medicine's bag still up when STRENGTH came to
    /// the party menu, and Start does nothing there. B backs out first.
    /// Fleet continue-1 in Rock Tunnel: FLASH used with the cave already
    /// lit; the game answers "This is in use already." on the party menu.
    /// Nothing to do: the menu is closed and the step is done.
    #[test]
    fn flash_in_use_already_is_nothing_to_do() {
        let mut step = PartyFieldMove::new(FieldMove::Flash, 0);
        step.chosen = true;
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        let mut menu = party_menu(0, &[], None);
        menu.prompt = "This is in use already.".into();
        o.party_menu = Some(menu);
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 60));
        assert!(step.nothing_there);
        assert_eq!(pressed(&d), Some(Button::B), "the menu closed");
    }

    #[test]
    fn a_bag_left_up_is_backed_out_of_before_the_start_menu() {
        let mut step = PartyFieldMove::new(FieldMove::Strength, 0);
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        o.bag = Some(pokebot_state::BagObservation {
            pocket: "ITEMS".into(),
            rows: vec![("HYPER POTION".into(), Some(1))],
            cursor: Some(0),
            prompt: None,
        });
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 60));
        assert_eq!(pressed(&d), Some(Button::B));
    }

    /// Party menu path (as recorded: the ▶ on SUMMARY, CUT the second
    /// row): select the member, open its actions, move to the move, A.
    #[test]
    fn the_party_menu_path_picks_the_members_move_row() {
        let mut step = PartyFieldMove::new(FieldMove::Flash, 3);
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        o.party_menu = Some(party_menu(0, &[], None));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::Down));
        o.party_menu = Some(party_menu(3, &[], None));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        o.party_menu = Some(party_menu(
            3,
            &["SUMMARY", "FLASH", "SWITCH", "ITEM", "CANCEL"],
            Some(0),
        ));
        let Decision::Act(a) = step.next(&mut step_ctx(&o, &state, &mut events, 0)) else {
            panic!()
        };
        assert_eq!(a.expect, Expectation::PartyOptionAt(1));
        o.party_menu = Some(party_menu(
            3,
            &["SUMMARY", "FLASH", "SWITCH", "ITEM", "CANCEL"],
            Some(1),
        ));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        // Back in the overworld and quiet: done.
        let mut o = bare(200);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "RockTunnel_1F".into(),
                x: 1,
                y: 1,
            },
            score: 1000,
        });
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 60)),
            Decision::Done(_)
        ));
    }

    /// Fleet continue-4: FLY on the last slot. The action window covers the
    /// bottom of the list, so the highlight under it reads as nobody; the
    /// window is the one A was pressed on, not another member's.
    #[test]
    fn the_last_members_window_hides_its_highlight() {
        let mut step = PartyFieldMove::new(FieldMove::Fly, 5);
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        let mut list = party_menu(5, &[], None);
        list.count = 6;
        o.party_menu = Some(list);
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        let mut window = party_menu(5, &["SUMMARY", "FLY", "SWITCH", "ITEM", "CANCEL"], Some(0));
        window.count = 6;
        window.selected = None;
        o.party_menu = Some(window);
        let Decision::Act(a) = step.next(&mut step_ctx(&o, &state, &mut events, 0)) else {
            panic!("the window is used")
        };
        assert_eq!(a.expect, Expectation::PartyOptionAt(1));
    }

    /// DIG asks "Want to escape from here and return to ROUTE 4?" over the
    /// party menu: YES, then the menu closes and the fade takes over.
    #[test]
    fn dig_answers_yes_to_leaving() {
        let mut step = PartyFieldMove::new(FieldMove::Dig, 0);
        step.unlocated_ok = true;
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        o.party_menu = Some(party_menu(
            0,
            &["SUMMARY", "DIG", "SWITCH", "ITEM", "CANCEL"],
            Some(1),
        ));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        let mut o = bare(40);
        o.party_menu = Some(party_menu(0, &[], None));
        o.menu = Some(yes_no(1));
        o.menu_lines = vec!["YES".into(), "NO".into()];
        let Decision::Act(a) = step.next(&mut step_ctx(&o, &state, &mut events, 0)) else {
            panic!("the question is answered")
        };
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::Up)]);
        o.menu = Some(yes_no(0));
        let Decision::Act(a) = step.next(&mut step_ctx(&o, &state, &mut events, 0)) else {
            panic!("YES is chosen")
        };
        assert_eq!(a.label, "YES: dig out");
        // The fade: nobody located, and done once quiet.
        let o = bare(200);
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 60)),
            Decision::Done(_)
        ));
    }

    /// A member whose actions lack the move: the party menu is closed and
    /// the step fails (the move is not known after all).
    #[test]
    fn a_member_without_the_move_fails_after_closing_the_menu() {
        let mut step = PartyFieldMove::new(FieldMove::Cut, 1);
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        o.party_menu = Some(party_menu(
            1,
            &["SUMMARY", "SWITCH", "ITEM", "CANCEL"],
            Some(0),
        ));
        let _ = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::B));
        let mut o = bare(300);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "CeruleanCity".into(),
                x: 1,
                y: 1,
            },
            score: 1000,
        });
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 60)),
            Decision::Fail(_)
        ));
    }

    /// Switch, Vermilion: the Cut tree was already cut; CUT from the list
    /// printed "There's nothing to CUT." in the prompt, the list stayed
    /// open, and the walk to the Gym failed "no progress in after". The
    /// way is clear: the menu is closed and the step is done.
    #[test]
    fn nothing_to_cut_means_the_way_is_clear() {
        let mut step = PartyFieldMove::new(FieldMove::Cut, 4);
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        o.party_menu = Some(party_menu(
            4,
            &["SUMMARY", "CUT", "SWITCH", "ITEM", "CANCEL"],
            Some(1),
        ));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        let mut o = bare(40);
        let mut list = party_menu(4, &[], None);
        list.prompt = "There’s nothing to CUT.".into();
        o.party_menu = Some(list);
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::B));
        assert!(step.nothing_there);
        let o = bare(80);
        match step.next(&mut step_ctx(&o, &state, &mut events, 0)) {
            Decision::Done(why) => assert!(why.contains("the way is clear"), "{why}"),
            _ => panic!("expected done"),
        }
    }

    /// Switch: the flight to Lavender landed, but the tracker, still on
    /// Celadon, found the player nowhere and the step failed "flying".
    /// The destination's name showing places the player at its landing.
    #[test]
    fn a_flight_lands_where_the_destination_is_named() {
        let landing = pokebot_state::PlayerPose {
            map: "CeruleanCity".into(),
            x: 22,
            y: 20,
        };
        let mut step = FlyTo::new("CeruleanCity")
            .unwrap()
            .landing_at(landing.clone());
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        o.fly_map = Some(FlyMapObservation {
            lit: vec!["CeruleanCity".into()],
            dark: vec![],
            cursor: Some((14, 3)),
        });
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        // Another town's name is not the landing.
        let mut elsewhere = bare(300);
        elsewhere.map_popup = Some("ROUTE 4".into());
        assert!(!matches!(
            step.next(&mut step_ctx(&elsewhere, &state, &mut events, 0)),
            Decision::Done(_)
        ));
        let mut landed = bare(310);
        landed.map_popup = Some("CERULEAN CITY".into());
        assert!(matches!(
            step.next(&mut step_ctx(&landed, &state, &mut events, 0)),
            Decision::Done(_)
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            GameEvent::PlayerInferred { pose, .. } if *pose == landing
        )));
        assert!(!step.names_destination("INDIGO PLATEAU"));
        let indigo = FlyTo::new("IndigoPlateau_Exterior").unwrap();
        assert!(indigo.names_destination("INDIGO PLATEAU"));
    }

    #[test]
    fn the_fly_cursor_walks_to_the_destination_cell() {
        let mut step = FlyTo::new("CeruleanCity").unwrap();
        let state = GameState::default();
        let mut events = Vec::new();
        let mut o = bare(1);
        let map = |cursor| FlyMapObservation {
            lit: vec!["PalletTown".into(), "CeruleanCity".into()],
            dark: vec![],
            cursor,
        };
        // From Pallet (4, 11): right first, then up.
        o.fly_map = Some(map(Some((4, 11))));
        let Decision::Act(a) = step.next(&mut step_ctx(&o, &state, &mut events, 0)) else {
            panic!()
        };
        assert_eq!(a.expect, Expectation::FlyCursorAt(5, 11));
        o.fly_map = Some(map(Some((14, 11))));
        let Decision::Act(a) = step.next(&mut step_ctx(&o, &state, &mut events, 0)) else {
            panic!()
        };
        assert_eq!(a.expect, Expectation::FlyCursorAt(14, 10));
        o.fly_map = Some(map(Some((14, 3))));
        let d = step.next(&mut step_ctx(&o, &state, &mut events, 0));
        assert_eq!(pressed(&d), Some(Button::A));
        // Landed on Cerulean: done.
        let mut o = bare(400);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "CeruleanCity".into(),
                x: 22,
                y: 20,
            },
            score: 1000,
        });
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Done(_)
        ));
        // A dark destination is refused, once the blinking icons had time
        // to show (the Switch drew them one frame in three).
        let mut step = FlyTo::new("FuchsiaCity").unwrap();
        let mut o = bare(1);
        o.fly_map = Some(map(Some((4, 11))));
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Wait(_)
        ));
        let mut o = bare(1 + FLY_ICON_WAIT_FRAMES);
        o.fly_map = Some(map(Some((4, 11))));
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Fail(_)
        ));
        // A lit one seen dark on a frame (its icon blinked off) waits, then
        // flies once the icon shows.
        let mut step = FlyTo::new("CeruleanCity").unwrap();
        let mut o = bare(1);
        o.fly_map = Some(FlyMapObservation {
            lit: vec![],
            dark: vec!["CeruleanCity".into()],
            cursor: Some((4, 11)),
        });
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Wait(_)
        ));
        o.frame_id = 10;
        o.fly_map = Some(map(Some((4, 11))));
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Act(_)
        ));
    }

    #[test]
    fn the_carrier_is_the_first_member_knowing_the_move() {
        let mon = |moves: &[&str]| {
            let mut m = PartyMon::default();
            for (i, mv) in moves.iter().enumerate() {
                m.moves[i] = Some(MoveSlot {
                    mv: Knowledge::observed((*mv).to_owned(), 1),
                    pp: Knowledge::observed((10, 10), 1),
                });
            }
            m
        };
        let mut state = GameState::default();
        assert_eq!(carrier(&state, "MOVE_CUT"), None);
        state.party = Knowledge::observed(
            vec![
                mon(&["MOVE_TACKLE"]),
                mon(&["MOVE_FLASH", "MOVE_CUT"]),
                mon(&["MOVE_CUT"]),
            ],
            1,
        );
        assert_eq!(carrier(&state, "MOVE_CUT"), Some(1));
        assert_eq!(carrier(&state, "MOVE_FLASH"), Some(1));
        assert_eq!(carrier(&state, "MOVE_SURF"), None);
        assert_eq!(
            FieldMove::from_move("MOVE_ROCK_SMASH"),
            Some(FieldMove::RockSmash)
        );
        assert!(says_used("PARAS used ROCK SMASH!", FieldMove::RockSmash));
        assert!(says_refused("Can’t use that here."));
        assert!(says_used(
            "IVYSAUR's STRENGTH made it possible to move boulders around!",
            FieldMove::Strength
        ));
    }

    /// Switch, Victory Road 1F: a push held the direction 400 ms, and the
    /// player walked on into the boulder's new tile after it. A push is
    /// one tap.
    #[test]
    fn a_push_is_one_tap() {
        let Some(world) = world() else { return };
        let mut push = Push {
            dir: Direction::Right,
            from: pokebot_state::PlayerPose {
                map: "VictoryRoad_1F".into(),
                x: 10,
                y: 18,
            },
            boulder: (11, 18),
            world,
            done: false,
            tries: 0,
            tapped: false,
            battles: 0,
            tapped_at: 0,
        };
        let state = GameState::default();
        let mut events = Vec::new();
        let o = bare(1);
        let Decision::Act(a) = push.next(&mut step_ctx(&o, &state, &mut events, 60)) else {
            panic!("expected the push")
        };
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::Right)]);
    }

    /// Fleet continue-5, Victory Road 1F: flown to Viridian to heal between
    /// pushes, back, and the pushes went on from where the boulder had
    /// been, STRENGTH off ("the boulder did not move Down"). Entered again,
    /// the sequence fails so as to start over.
    #[test]
    fn leaving_the_map_between_pushes_starts_them_over() {
        assert!(left_mid_push(3, 3).is_ok());
        match left_mid_push(3, 5) {
            Err(ToolError::Failed(why)) => assert!(why.starts_with(LEFT_MID_PUSH), "{why}"),
            r => panic!("expected a failure, got {r:?}"),
        }
    }

    /// Fleet continue-6, Victory Road 3F: the player at (5, 7), the boulder
    /// pushed from (6, 7) onto the switch at (7, 7). The player walks in
    /// place; the old tile shows bare floor, the new one the boulder: the
    /// push happened (judged by the player's step, it "did not move").
    #[test]
    fn a_push_is_judged_by_the_boulder_not_the_player() {
        let Some(world) = world() else { return };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(image) = pokebot_video::png::load(
            root.join("captures/fixtures/emu-vr3f-boulder-pushed-onto-switch.png"),
        ) else {
            return;
        };
        let Ok(frame) = pokebot_core::NormalizedFrame::new(1, std::time::Instant::now(), image)
        else {
            return;
        };
        let mut o = bare(1);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "VictoryRoad_3F".into(),
                x: 5,
                y: 7,
            },
            score: 1000,
        });
        assert!(pushed(&world, &o, Some(&frame), (6, 7), Direction::Right));
        // Not from (7, 7) on: nothing went to (8, 7).
        assert!(!pushed(&world, &o, Some(&frame), (7, 7), Direction::Right));
        // Fleet continue-2, Victory Road 2F: pushed Down from (6, 17), the
        // player above at (6, 16). The boulder on (6, 18) is drawn over
        // (6, 17)'s bottom half; its top half is bare floor.
        let Ok(image) = pokebot_video::png::load(
            root.join("captures/fixtures/emu-vr2f-boulder-pushed-down.png"),
        ) else {
            return;
        };
        let Ok(frame) = pokebot_core::NormalizedFrame::new(2, std::time::Instant::now(), image)
        else {
            return;
        };
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "VictoryRoad_2F".into(),
                x: 6,
                y: 16,
            },
            score: 1000,
        });
        assert!(pushed(&world, &o, Some(&frame), (6, 17), Direction::Down));
        assert!(!pushed(&world, &o, Some(&frame), (6, 18), Direction::Down));
    }

    /// Fleet continue-1, Victory Road 1F: wild battles came between the
    /// taps and the judging, each counted as a push that failed, and the
    /// boulder was never pushed. A tap a battle came after is sent again,
    /// not counted.
    #[test]
    fn a_tap_lost_to_a_battle_is_not_a_try() {
        let Some(world) = world() else { return };
        let mut push = Push {
            dir: Direction::Down,
            from: pokebot_state::PlayerPose {
                map: "VictoryRoad_1F".into(),
                x: 7,
                y: 17,
            },
            boulder: (7, 18),
            world,
            done: false,
            tries: 0,
            tapped: false,
            battles: 0,
            tapped_at: 0,
        };
        let state = GameState::default();
        let mut events = Vec::new();
        let o = bare(1);
        let a = match push.next(&mut step_ctx(&o, &state, &mut events, 60)) {
            Decision::Act(a) => a,
            _ => panic!("expected the tap"),
        };
        push.on_outcome(
            &a,
            Outcome::Confirmed,
            &mut step_ctx(&o, &state, &mut events, 60),
        );
        let learned = [GameEvent::BattleStarted];
        let mut ctx = StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: 60,
            frame: None,
            learned: &learned,
        };
        assert!(matches!(push.next(&mut ctx), Decision::Wait(_)));
        assert_eq!(push.tries, 0);
        assert!(
            matches!(push.next(&mut ctx), Decision::Act(_)),
            "tapped again"
        );
    }

    /// Fleet continue-1, Victory Road 1F: a push Right judged mid-slide,
    /// the boulder still half over the tile it left. The judging waits for
    /// the slide to end.
    #[test]
    fn a_push_is_judged_once_the_boulder_stops() {
        let Some(world) = world() else { return };
        let mut push = Push {
            dir: Direction::Right,
            from: pokebot_state::PlayerPose {
                map: "VictoryRoad_1F".into(),
                x: 6,
                y: 19,
            },
            boulder: (7, 19),
            world,
            done: false,
            tries: 0,
            tapped: false,
            battles: 0,
            tapped_at: 0,
        };
        let state = GameState::default();
        let mut events = Vec::new();
        let o = bare(100);
        let a = match push.next(&mut step_ctx(&o, &state, &mut events, 60)) {
            Decision::Act(a) => a,
            _ => panic!("expected the tap"),
        };
        push.on_outcome(
            &a,
            Outcome::Confirmed,
            &mut step_ctx(&o, &state, &mut events, 60),
        );
        let o = bare(130);
        assert!(matches!(
            push.next(&mut step_ctx(&o, &state, &mut events, 60)),
            Decision::Wait(_)
        ));
        // Stopped, nothing to judge by: the tap counts.
        let o = bare(170);
        assert!(matches!(
            push.next(&mut step_ctx(&o, &state, &mut events, 60)),
            Decision::Done(_)
        ));
    }

    /// Switch, Victory Road 1F, after the first push down: the player at
    /// (7, 18), the boulder below at (7, 19). The sensor doesn't read a
    /// moved boulder, but its tile doesn't look like the map's floor; the
    /// floor beside it does.
    #[test]
    fn a_boulders_tile_is_not_bare_floor() {
        let Some(world) = world() else { return };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(image) = pokebot_video::png::load(
            root.join("captures/fixtures/switch-vr1f-boulder-below-player.png"),
        ) else {
            return;
        };
        let Ok(frame) = pokebot_core::NormalizedFrame::new(1, std::time::Instant::now(), image)
        else {
            return;
        };
        let mut o = bare(1);
        o.player = Some(pokebot_state::PoseObservation {
            pose: pokebot_state::PlayerPose {
                map: "VictoryRoad_1F".into(),
                x: 7,
                y: 18,
            },
            score: 1000,
        });
        assert_eq!(bare_top(&world, &o, Some(&frame), (7, 19)), Some(false));
        assert_eq!(bare_top(&world, &o, Some(&frame), (6, 19)), Some(true));
    }

    /// Switch, Victory Road 1F: between pushes the walk to the tile behind
    /// the boulder went through the boulder's own tile, which with
    /// Strength on pushes it. The walk round treats the boulder as a wall:
    /// from (11, 19) to (10, 18) with the boulder at (11, 18), the route
    /// doesn't cross (11, 18).
    #[test]
    fn the_walk_between_pushes_goes_round_the_boulder() {
        let Some(world) = world() else { return };
        let nav = NavParts {
            story: None,
            world: std::sync::Arc::clone(&world),
            gone: Default::default(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
            data: None,
        };
        let walled = boulder_walled(nav, "VictoryRoad_1F", (11, 18));
        assert!(walled
            .gates
            .closed_on("VictoryRoad_1F")
            .any(|t| t == (11, 18)));
        let pose = pokebot_state::PlayerPose {
            map: "VictoryRoad_1F".into(),
            x: 11,
            y: 19,
        };
        let map = world.map("VictoryRoad_1F").unwrap();
        let mut obstacles: pokebot_world::path::Obstacles =
            walled.gates.closed_on("VictoryRoad_1F").collect();
        obstacles.remove(&(pose.x, pose.y));
        let walk = pokebot_world::path::Walk {
            obstacles: &obstacles,
            surf: false,
            opened: None,
        };
        let path = pokebot_world::path::reach(map, (11, 19), &walk, |_| 0)
            .path((10, 18))
            .expect("a way round");
        assert!(path.iter().all(|s| s.to != (11, 18)), "{path:?}");
    }

    /// Fleet continue-1, Victory Road 3F: once pushed off (32, 5), the
    /// boulder's map object no longer stands there; the walk to the next
    /// push may cross its first tile, and goes round where it is now.
    #[test]
    fn a_pushed_boulders_first_tile_is_free() {
        let Some(world) = world() else { return };
        let nav = NavParts {
            story: None,
            world: std::sync::Arc::clone(&world),
            gone: Default::default(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
            data: None,
        };
        let map = world.map("VictoryRoad_3F").unwrap();
        let held = crate::nav::object_obstacles(map, &nav.gone);
        assert!(held.contains(&(32, 5)), "the object holds its spawn");
        let walled = pushed_boulder_walled(nav, "VictoryRoad_3F", (32, 5), (31, 5));
        let free = crate::nav::object_obstacles(map, &walled.gone);
        assert!(!free.contains(&(32, 5)));
        assert!(walled
            .gates
            .closed_on("VictoryRoad_3F")
            .any(|t| t == (31, 5)));
    }

    /// Switch, Victory Road 1F: a wild battle cut a push short and the
    /// reckoned pose counted it done. The boulder's sprite decides: still
    /// on its tile, it didn't move; on the next one, it did; out of
    /// sight, the player's move is taken.
    #[test]
    fn a_push_counts_only_when_the_boulder_moved() {
        let mut o = bare(1);
        let sprite = |x, y| pokebot_state::SpriteObservation {
            x,
            y,
            local_id: None,
            facing: None,
        };
        o.sprites = vec![sprite(7, 18)];
        assert_eq!(boulder_moved(&o, (7, 18), (7, 19)), Some(false));
        o.sprites = vec![sprite(7, 19)];
        assert_eq!(boulder_moved(&o, (7, 18), (7, 19)), Some(true));
        o.sprites = Vec::new();
        assert_eq!(boulder_moved(&o, (7, 18), (7, 19)), None);
    }

    /// Switch, Victory Road 1F: the barrier at (12, 14) opens when a
    /// boulder lands on the floor switch at (20, 16). From the entrance the
    /// pushes are found, each from a tile the player walks to, the last
    /// landing the boulder on the switch.
    #[test]
    fn a_boulder_is_pushed_onto_the_floor_switch() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = pokebot_world::World::load(&dir) else {
            return;
        };
        let map = world.map("VictoryRoad_1F").unwrap();
        let switch = (20, 16);
        assert_eq!(
            map.tile(switch.0, switch.1).unwrap().behavior,
            pokebot_world::behavior::STRENGTH_BUTTON
        );
        let boulders: Vec<(i32, i32)> = map
            .objects
            .iter()
            .filter(|o| o.graphics.as_deref() == Some("OBJ_EVENT_GFX_PUSHABLE_BOULDER"))
            .filter_map(|o| Some((o.x?, o.y?)))
            .collect();
        let mut blocked = crate::nav::object_obstacles(map, &Default::default());
        for b in &boulders {
            blocked.remove(b);
        }
        let (boulder, pushes) =
            boulder_pushes(map, &boulders, &blocked, (11, 19), switch).expect("pushes");
        eprintln!("{boulder:?} {pushes:?}");
        let end = pushes.iter().fold(boulder, |(x, y), d| {
            let (dx, dy) = d.delta();
            (x + dx, y + dy)
        });
        assert_eq!(end, switch);
    }

    /// Switch, Victory Road 2F: the pushes took the boulder at (6, 17) up
    /// the stairs at (9, 12) onto the platform (elevation 4), then had the
    /// player push it Down from the cave floor at (9, 9) (elevation 3): the
    /// game stops that step at the elevation mismatch and the boulder never
    /// moved. Every push keeps the player and the boulder each on one level;
    /// the boulder on the switch's row, below the platform, is the one.
    #[test]
    fn a_boulder_is_not_pushed_across_an_elevation_change() {
        use pokebot_world::path::walkable_elevation;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = pokebot_world::World::load(&dir) else {
            return;
        };
        let map = world.map("VictoryRoad_2F").unwrap();
        let switch = (14, 19);
        let boulders: Vec<(i32, i32)> = map
            .objects
            .iter()
            .filter(|o| o.graphics.as_deref() == Some("OBJ_EVENT_GFX_PUSHABLE_BOULDER"))
            .filter_map(|o| Some((o.x?, o.y?)))
            .collect();
        let mut blocked = crate::nav::object_obstacles(map, &Default::default());
        for b in &boulders {
            blocked.remove(b);
        }
        let (boulder, pushes) =
            boulder_pushes(map, &boulders, &blocked, (5, 17), switch).expect("pushes");
        let elevation = |(x, y): (i32, i32)| map.tile(x, y).unwrap().elevation;
        let mut at = boulder;
        for d in &pushes {
            let (dx, dy) = d.delta();
            let behind = (at.0 - dx, at.1 - dy);
            let next = (at.0 + dx, at.1 + dy);
            assert!(
                walkable_elevation(elevation(behind), elevation(at)),
                "{d:?} from {behind:?} onto the boulder at {at:?}"
            );
            assert!(walkable_elevation(elevation(at), elevation(next)));
            at = next;
        }
        assert_eq!(at, switch);
        // The boulder on the switch's own row, not the one by the stairs.
        assert_eq!(boulder, (33, 19));
    }
}
