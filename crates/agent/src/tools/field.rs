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
}

impl FieldMove {
    pub const ALL: [FieldMove; 7] = [
        FieldMove::Cut,
        FieldMove::Fly,
        FieldMove::Surf,
        FieldMove::Strength,
        FieldMove::Flash,
        FieldMove::RockSmash,
        FieldMove::Waterfall,
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
        }
    }

    /// The badge that allows the move outside battle (FireRed:
    /// `FLAG_BADGE0n_GET` checked by the field scripts and the party menu).
    pub fn badge(self) -> u8 {
        match self {
            FieldMove::Flash => 1,
            FieldMove::Cut => 2,
            FieldMove::Fly => 3,
            FieldMove::Strength => 4,
            FieldMove::Surf => 5,
            FieldMove::RockSmash => 6,
            FieldMove::Waterfall => 7,
        }
    }

    /// Pressing A on the obstacle offers the move.
    pub fn has_overworld_prompt(self) -> bool {
        !matches!(self, FieldMove::Fly | FieldMove::Flash)
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
            return crate::new_game::advance_or_wait(o.dialogue.as_ref(), "reading");
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
    /// Action windows closed because another member's opened.
    wrong_member: u32,
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
            wrong_member: 0,
        }
    }

    fn after_choice(&mut self, o: &Observation, quiet: u32) -> Decision {
        if self.mv == FieldMove::Fly && o.fly_map.is_some() {
            return Decision::Done("region map open".into());
        }
        if let Some(d) = &o.dialogue {
            let page = d.lines.join(" ");
            if !page.is_empty() && self.pages.last() != Some(&page) {
                if says_refused(&page) || page.contains("not able") {
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
            // Refusals print in the party menu's prompt box.
            if says_refused(&party.prompt) {
                self.failed = Some(party.prompt.clone());
                return Decision::Wait("refused".into());
            }
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
                if party.selected != Some(self.slot) {
                    // The window of another member: back to the list.
                    self.wrong_member += 1;
                    if self.wrong_member > 3 {
                        return Decision::Fail(format!(
                            "the action window never belonged to slot {} (read {:?})",
                            self.slot, party.selected
                        ));
                    }
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
            return crate::new_game::advance_or_wait(o.dialogue.as_ref(), "reading");
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

/// On the region map: walk the cursor to `cell`, press A, and wait until
/// the player is located on `map`.
pub struct FlyTo {
    map: String,
    cell: (u32, u32),
    retries: Retries,
    pressed: Option<u64>,
}

impl FlyTo {
    pub fn new(map: &str) -> Option<Self> {
        Some(Self {
            map: map.to_owned(),
            cell: fly_map::spot_cell(map)?,
            retries: Retries::default(),
            pressed: None,
        })
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
        let Some(map) = &o.fly_map else {
            if self.pressed.is_some() {
                return Decision::Wait("flying".into());
            }
            return self.retries.wait(o, "waiting for the region map");
        };
        if !map.lit.iter().any(|m| m == &self.map) {
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
    done: bool,
    tries: u32,
}

impl ToolStep for Push {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        if self.done {
            return Decision::Done("pushed".into());
        }
        if self.tries >= 3 {
            return Decision::Fail(format!("the boulder did not move {:?}", self.dir));
        }
        if ctx.quiet_frames < 20 {
            return Decision::Wait("settling before the push".into());
        }
        Decision::Act(Action::new(
            format!("push the boulder {:?}", self.dir),
            vec![ControllerCommand::Hold {
                buttons: direction_button(self.dir).into(),
                duration: std::time::Duration::from_millis(400),
            }],
            Expectation::PlayerMovedFrom(self.from.clone()),
            90,
        ))
    }

    fn on_outcome(&mut self, _: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome == Outcome::Confirmed {
            self.done = true;
        } else {
            self.tries += 1;
        }
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
            Ok(())
        }
        Err(e) => Err(e),
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

/// Flies to `map` (a lit fly spot).
pub fn fly(ctx: &mut ToolContext<'_>, map: &str) -> Result<(), ToolError> {
    let mut to = FlyTo::new(map)
        .ok_or_else(|| ToolError::Failed(format!("{map} is not a fly spot on the region map")))?;
    use_from_party(ctx, FieldMove::Fly)?;
    ctx.drive(&mut to)?;
    ctx.emit(GameEvent::MapVisited {
        map: map.to_owned(),
    })?;
    Ok(())
}

/// Activates Strength on the boulder at `(x, y)` of `map`, then pushes it
/// along `pushes`, walking behind it before each push.
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
    let mut boulder = (x, y);
    for &dir in pushes {
        let (dx, dy) = dir.delta();
        let behind = (boulder.0 - dx, boulder.1 - dy);
        super::go::go(
            ctx,
            Destination::Tile {
                map: map.to_owned(),
                x: behind.0,
                y: behind.1,
            },
        )?;
        let from = ctx
            .pose()
            .ok_or_else(|| ToolError::Failed("not located behind the boulder".into()))?;
        let mut push = Push {
            dir,
            from,
            done: false,
            tries: 0,
        };
        ctx.drive(&mut push)?;
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
            (FieldMove::Flash, _) => use_from_party(ctx, field),
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
            world,
            gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
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
    #[test]
    fn a_tree_that_is_not_offered_fails_the_prompt() {
        let Some(world) = world() else { return };
        let nav = NavParts {
            world,
            gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
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
        }
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
        // A dark destination is refused.
        let mut step = FlyTo::new("FuchsiaCity").unwrap();
        let mut o = bare(1);
        o.fly_map = Some(map(Some((4, 11))));
        assert!(matches!(
            step.next(&mut step_ctx(&o, &state, &mut events, 0)),
            Decision::Fail(_)
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
}
