//! Conversations (spec §7.4): read every page with the font, recognise the
//! text in `dialogue.json`, answer questions from the plan or from the
//! branch policy, and, once the conversation ends, record which compiled
//! script path ran and what it changed.
//!
//! [`Conversation`] is the step; [`DialogueTool`] serves `RunScript` and
//! `Unstick` drives the same step without a known script.

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, MenuObservation, Observation, PlayerPose, ScreenState};
use pokebot_world::dialogue::Dialogue;
use pokebot_world::events::{Condition, Effect, Script, Val};
use pokebot_world::predicate::{BeliefView, Truth};
use pokebot_world::route::requirement_of;
use pokebot_world::World;

use super::effects::{chained_scenes, path_events, path_labels, LabelIndex};
use super::go::{GoStep, NavParts};
use super::scene::SceneStep;
use super::talk::TalkStep;
use super::{
    progress, Answer, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep, SETTLE_FRAMES,
};
use crate::belief_view::StateBelief;
use crate::nav::Destination;
use crate::new_game::{advance_or_wait, select};
use crate::track::TextTracker;
use crate::{Action, Decision, Expectation, Outcome};

/// Frames a page's text must stay unchanged to count as fully printed.
const PAGE_PRINTED_FRAMES: u32 = 8;
/// Longest wait for a page waiting for A to read the same on a second
/// frame (a flickering misread must not stall the conversation).
const PAGE_READ_WAIT_FRAMES: u64 = 10;
/// Pages of an unrecognised conversation before its text and frame are
/// saved for the overlay (§7.4 step 4).
const UNKNOWN_AFTER_PAGES: usize = 3;

/// Characters other than `*` a label's matched text must have: a label
/// that is all placeholders (`{STR_VAR_1}`) matches anything and means
/// nothing.
const MIN_CONCRETE_CHARS: usize = 4;

/// Whether the first `lines` lines of `label` say something concrete.
fn concrete(dialogue: &Dialogue, label: &str, lines: usize) -> bool {
    dialogue.labels.get(label).is_some_and(|pages| {
        pages
            .iter()
            .flatten()
            .take(lines.max(1))
            .flat_map(|l| l.chars())
            .filter(|c| *c != '*' && !c.is_whitespace())
            .count()
            >= MIN_CONCRETE_CHARS
    })
}

/// The labels whose text starts with `lines` (sorted), placeholder-only
/// labels left out.
fn identify_lines(dialogue: &Dialogue, lines: &[String]) -> Vec<String> {
    dialogue
        .identify(lines)
        .into_iter()
        .filter(|label| concrete(dialogue, label, lines.len()))
        .map(str::to_owned)
        .collect()
}

/// The lines on screen matched against the game text: the labels whose
/// text starts with them (sorted). Empty without a dialogue box or a font.
pub fn identify(observation: &Observation, dialogue: &Dialogue) -> Vec<String> {
    let Some(d) = &observation.dialogue else {
        return Vec::new();
    };
    identify_lines(dialogue, &d.lines)
}

/// The YES/NO conditions along a path, in order: `true` for the branch
/// taken with YES. A test against 0 (`eq 0`: YES, `ne 0`: not YES) opens
/// a question; the tests against 1 and CANCEL that follow it (`ne 1`,
/// `eq CANCEL`: the compiler's elimination of the other options) belong
/// to the same question.
pub fn question_branches(when: &[Condition]) -> Vec<bool> {
    let mut out = Vec::new();
    let mut open = false;
    for c in when {
        match c {
            Condition::Answer { answer } => {
                open = false;
                out.push(answer.eq_ignore_ascii_case("yes"));
            }
            Condition::Choice { choice, cmp } if choice == "MULTICHOICE_YES_NO" => {
                let is = |v: &Option<Val>, n: i64| v.as_ref().and_then(Val::as_int) == Some(n);
                if is(&cmp.eq, 0) {
                    out.push(true);
                    open = true;
                } else if is(&cmp.ne, 0) {
                    out.push(false);
                    open = true;
                } else if !open {
                    // A question tested on NO first.
                    if is(&cmp.eq, 1) {
                        out.push(false);
                    } else if is(&cmp.ne, 1) {
                        out.push(true);
                    }
                    open = true;
                }
            }
            _ => open = false,
        }
    }
    out
}

/// Whether the branch would spend money or items.
fn spends(does: &[Effect]) -> bool {
    does.iter().any(|e| match e {
        Effect::Take { .. } => true,
        Effect::Money { money } => money.as_int().is_some_and(|m| m < 0),
        Effect::Mart { .. } => true,
        _ => false,
    })
}

/// Whether the branch heals or gives something for free.
fn gives(does: &[Effect]) -> bool {
    does.iter().any(|e| match e {
        Effect::Heal { heal } => *heal,
        Effect::Give { count, .. } => *count > 0,
        Effect::GiveMon { .. } | Effect::GiveEgg { .. } => true,
        Effect::Money { money } => money.as_int().is_some_and(|m| m > 0),
        _ => false,
    })
}

/// The branch policy for a YES/NO the plan didn't expect (§7.4 step 2):
/// among the paths of `script` consistent with the answers given so far,
/// look at what YES does: NO when it would spend money or items, YES when
/// it heals or gives something for free, NO otherwise. Without a script:
/// NO.
pub fn policy_answer(script: Option<&Script>, answered: &[bool]) -> Answer {
    let Some(script) = script else {
        return Answer::No;
    };
    let k = answered.len();
    let yes_branches: Vec<&[Effect]> = script
        .paths
        .iter()
        .filter_map(|p| {
            let branches = question_branches(&p.when);
            (branches.len() > k && branches[..k] == *answered && branches[k])
                .then_some(p.does.as_slice())
        })
        .collect();
    if yes_branches.is_empty() {
        return Answer::No;
    }
    if yes_branches.iter().any(|d| spends(d)) {
        return Answer::No;
    }
    if yes_branches.iter().any(|d| gives(d)) {
        return Answer::Yes;
    }
    Answer::No
}

/// The path of `script` that printed `labels` and took `answered`: the
/// consistent path printing the most of them (ties: the lowest index).
/// With no labels recognised, the only consistent path, if there is one.
pub fn resolve_path(script: &Script, labels: &[String], answered: &[bool]) -> Option<usize> {
    resolve_path_in(script, labels, answered, None)
}

/// [`resolve_path`] among the paths whose conditions `belief` doesn't know
/// to be false (the rival's battle path for the starter the player took).
pub fn resolve_path_in(
    script: &Script,
    labels: &[String],
    answered: &[bool],
    belief: Option<&dyn BeliefView>,
) -> Option<usize> {
    let possible = |p: &pokebot_world::events::ScriptPath| {
        let Some(belief) = belief else { return true };
        match requirement_of(&p.when) {
            Some(req) => !req.iter().any(|q| belief.eval(q) == Truth::False),
            None => true,
        }
    };
    let consistent: Vec<(usize, usize)> = script
        .paths
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            let branches = question_branches(&p.when);
            branches.len() >= answered.len()
                && branches[..answered.len()] == *answered
                && possible(p)
        })
        .map(|(i, p)| {
            let printed = path_labels(&p.does);
            let score = labels
                .iter()
                .filter(|l| printed.contains(&l.as_str()))
                .count();
            (i, score)
        })
        .collect();
    if labels.is_empty() {
        return match consistent.as_slice() {
            [(i, _)] => Some(*i),
            _ => None,
        };
    }
    consistent
        .iter()
        .filter(|(_, score)| *score > 0)
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(i, _)| *i)
}

/// A conversation on screen, followed page by page.
pub struct Conversation {
    world: Arc<World>,
    data: Arc<GameData>,
    /// The compiled script known to be running, and its path when the plan
    /// knows it.
    pub script: Option<String>,
    pub path: Option<usize>,
    answers: Vec<Answer>,
    answers_used: usize,
    /// Answers given, YES = true.
    pub answered: Vec<bool>,
    /// Text labels recognised so far, in order.
    pub recognised: Vec<String>,
    /// Lines accumulated for the label being narrowed down (several
    /// candidates share the first line).
    pending_lines: Vec<String>,
    /// Every page applied, as read (for the unknown log).
    pub pages: Vec<String>,
    /// The page read on the previous observation and the last applied.
    candidate: Option<String>,
    last_page: String,
    unread_since: Option<u64>,
    tracker: TextTracker,
    /// The conversation saw at least one page.
    started: bool,
    /// The unknown conversation was logged.
    unknown_logged: bool,
    /// Frame and text to save (set once, taken by the tool).
    pub unknown: Option<(u64, String)>,
    /// An item was received in this conversation.
    pub gained_item: bool,
    /// Where the player stood when it started: tells apart scripts that
    /// print the same text (the three tiles of one trigger, one map's
    /// scene and another's).
    pub near: Option<PlayerPose>,
}

impl Conversation {
    pub fn new(
        world: Arc<World>,
        data: Arc<GameData>,
        script: Option<String>,
        path: Option<usize>,
        answers: Vec<Answer>,
    ) -> Self {
        Self {
            world,
            data,
            script,
            path,
            answers,
            answers_used: 0,
            answered: Vec::new(),
            recognised: Vec::new(),
            pending_lines: Vec::new(),
            pages: Vec::new(),
            candidate: None,
            last_page: String::new(),
            unread_since: None,
            tracker: TextTracker::default(),
            started: false,
            unknown_logged: false,
            unknown: None,
            gained_item: false,
            near: None,
        }
    }

    /// Started where the player stands at `pose`.
    pub fn near(mut self, pose: Option<PlayerPose>) -> Self {
        self.near = pose;
        self
    }

    fn script_data(&self) -> Option<&Script> {
        self.world.events()?.script(self.script.as_deref()?)
    }

    /// Recognition of a fully printed page.
    fn recognise(&mut self, lines: &[String]) {
        let Some(dialogue) = self.world.dialogue() else {
            return;
        };
        let mut lines: Vec<String> = lines.iter().map(|l| l.trim().to_owned()).collect();
        lines.retain(|l| !l.is_empty());
        if lines.is_empty() {
            return;
        }
        let mut accumulated = self.pending_lines.clone();
        accumulated.extend(lines.iter().cloned());
        let narrowed = identify_lines(dialogue, &accumulated);
        match narrowed.len() {
            1 => {
                self.recognised.push(narrowed[0].clone());
                self.pending_lines.clear();
                return;
            }
            n if n > 1 && !self.pending_lines.is_empty() => {
                self.pending_lines = accumulated;
                return;
            }
            _ => {}
        }
        // Not a continuation of the pending label: a new label starts here.
        let fresh = identify_lines(dialogue, &lines);
        match fresh.len() {
            0 => self.pending_lines.clear(),
            1 => {
                self.recognised.push(fresh[0].clone());
                self.pending_lines.clear();
            }
            _ => self.pending_lines = lines,
        }
    }

    /// The answer to the question on screen: the plan's next answer, else
    /// the branch policy.
    fn answer(&mut self, menu: &MenuObservation) -> Decision {
        if menu.rows > 2 {
            // A menu the plan doesn't need: B.
            return Decision::Act(Action::new(
                "close the menu (B)",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::MenuClosed,
                90,
            ));
        }
        let (answer, why) = match self.answers.get(self.answers_used) {
            Some(a) => (*a, "planned"),
            None => (policy_answer(self.script_data(), &self.answered), "policy"),
        };
        match answer {
            Answer::Yes => select(menu, 0, &format!("answer YES ({why})")),
            Answer::No => select(menu, 1, &format!("answer NO ({why})")),
        }
    }

    /// The script this conversation belongs to, when it can be told: the
    /// one given, else the only script printing every recognised label.
    pub fn resolved_script(&self, index: &LabelIndex) -> Option<String> {
        if let Some(s) = &self.script {
            return Some(s.clone());
        }
        if self.recognised.is_empty() {
            return None;
        }
        let candidates = index.scripts_for(&self.recognised);
        match candidates.as_slice() {
            [one] => Some(one.clone()),
            [] => None,
            _ => self.nearest_script(&candidates),
        }
    }

    /// Of scripts printing the same text, the one where the player is: a
    /// trigger under (or next to) the tile the conversation started on,
    /// else the only one on the player's map.
    fn nearest_script(&self, candidates: &[String]) -> Option<String> {
        let near = self.near.as_ref()?;
        let events = self.world.events()?;
        let close = |(x, y): (i32, i32)| (x - near.x).abs() + (y - near.y).abs() <= 1;
        let at: Vec<&String> = candidates
            .iter()
            .filter(|label| {
                events.triggers.iter().any(|t| {
                    t.map == near.map
                        && t.script.as_deref() == Some(label.as_str())
                        && close((t.x, t.y))
                })
            })
            .collect();
        if let [one] = at.as_slice() {
            // Under the player first, then beside.
            return Some((*one).clone());
        }
        if at.len() > 1 {
            let under: Vec<&&String> = at
                .iter()
                .filter(|label| {
                    events.triggers.iter().any(|t| {
                        t.map == near.map
                            && t.script.as_deref() == Some(label.as_str())
                            && (t.x, t.y) == (near.x, near.y)
                    })
                })
                .collect();
            if let [one] = under.as_slice() {
                return Some((**one).clone());
            }
        }
        let on_map: Vec<&String> = candidates
            .iter()
            .filter(|label| {
                events
                    .script(label)
                    .and_then(|s| s.map.as_deref())
                    .is_some_and(|m| m == near.map)
            })
            .collect();
        match on_map.as_slice() {
            [one] => Some((*one).clone()),
            _ => None,
        }
    }

    /// The (script, path) that ran, if it can be told.
    pub fn resolved(&self, index: &LabelIndex) -> Option<(String, usize)> {
        self.resolved_in(index, None)
    }

    /// [`Conversation::resolved`] among the paths `belief` allows.
    pub fn resolved_in(
        &self,
        index: &LabelIndex,
        belief: Option<&dyn BeliefView>,
    ) -> Option<(String, usize)> {
        let script = self.resolved_script(index)?;
        let Some(data) = self.world.events().and_then(|e| e.script(&script)) else {
            return self.path.map(|p| (script, p));
        };
        let Some(planned) = self.path else {
            let path = resolve_path_in(data, &self.recognised, &self.answered, belief)?;
            return Some((script, path));
        };
        // The plan's path, unless the text read is another path's: the
        // game took the branch it prints, whatever the plan expected (the
        // PC before Bill went into the teleporter shows the idle message,
        // not the Cell Separator's).
        if self.recognised.is_empty() {
            return Some((script, planned));
        }
        let score = |i: usize| {
            data.paths.get(i).map_or(0, |p| {
                let printed = path_labels(&p.does);
                self.recognised
                    .iter()
                    .filter(|l| printed.contains(&l.as_str()))
                    .count()
            })
        };
        let own = score(planned);
        if own == self.recognised.len() {
            return Some((script, planned));
        }
        // What the belief rules out may be what it got wrong: without it
        // when nothing else prints the text.
        let read = resolve_path_in(data, &self.recognised, &self.answered, belief)
            .or_else(|| resolve_path_in(data, &self.recognised, &self.answered, None));
        match read {
            Some(q) if score(q) > own => Some((script, q)),
            // None of the script's paths prints what was read.
            _ if own == 0 => None,
            _ => Some((script, planned)),
        }
    }
}

impl ToolStep for Conversation {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        let Some(d) = &o.dialogue else {
            if let Some(menu) = &o.menu {
                // A menu without text (a multichoice after the text closed,
                // or one we don't know): B, unless the plan is answering.
                if self.answers.get(self.answers_used).is_some() && menu.rows <= 2 {
                    return self.answer(menu);
                }
                return Decision::Act(Action::new(
                    "close an unexpected menu",
                    vec![ControllerCommand::Press(Button::B)],
                    Expectation::MenuClosed,
                    45,
                ));
            }
            if !self.started {
                return Decision::Wait("waiting for the conversation".into());
            }
            if ctx.quiet_frames >= SETTLE_FRAMES {
                return Decision::Done(format!(
                    "conversation over: {} pages, {} recognised",
                    self.pages.len(),
                    self.recognised.len()
                ));
            }
            return Decision::Wait("conversation ending".into());
        };
        self.started = true;
        let printed = d.ready_for_a() || o.menu.is_some() || d.stable_frames >= PAGE_PRINTED_FRAMES;
        if printed {
            let page = d.lines.join(" ");
            let confirmed = self.candidate.as_deref() == Some(page.as_str());
            self.candidate = Some(page.clone());
            if confirmed && !page.is_empty() && page != self.last_page {
                self.last_page = page.clone();
                self.pages.push(page);
                self.recognise(&d.lines);
                let tracked = self.tracker.observe_page(&d.lines, &self.data, None);
                if tracked
                    .iter()
                    .any(|e| matches!(e, GameEvent::ItemsChanged { delta, .. } if *delta > 0))
                {
                    self.gained_item = true;
                }
                ctx.events
                    .extend(tracked.into_iter().filter(crate::track::tool_emits));
                if self.recognised.is_empty()
                    && self.pages.len() >= UNKNOWN_AFTER_PAGES
                    && !self.unknown_logged
                {
                    self.unknown_logged = true;
                    self.unknown = Some((o.frame_id, self.pages.join("\n")));
                    ctx.events.push(progress(
                        "unknown_dialogue",
                        format!("{} pages unrecognised: {:?}", self.pages.len(), self.pages),
                    ));
                }
            }
        }
        if let Some(menu) = &o.menu {
            return self.answer(menu);
        }
        // A page waiting for A is read on two frames before it is advanced,
        // so its facts (items, money) are not lost to the executor's
        // pending window; a flickering misread gives up after a while.
        if d.ready_for_a() && !self.tracker.applied(&d.lines) {
            let since = *self.unread_since.get_or_insert(o.frame_id);
            if o.frame_id.saturating_sub(since) < PAGE_READ_WAIT_FRAMES {
                return Decision::Wait("reading the page on a second frame".into());
            }
        }
        self.unread_since = None;
        advance_or_wait(Some(d), "dialogue")
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _ctx: &mut StepContext<'_>) {
        if outcome == Outcome::Confirmed && action.label.starts_with("answer ") {
            let yes = action.label.starts_with("answer YES");
            self.answered.push(yes);
            if self.answers.get(self.answers_used).is_some() {
                self.answers_used += 1;
            }
        }
    }

    fn expects(&self) -> Expects {
        Expects::DIALOGUE
    }
}

/// Finishes a conversation: saves an unrecognised one, and records the
/// path that ran with its effects. What was recognised, for the log.
pub fn finish(ctx: &mut ToolContext<'_>, conversation: &Conversation) -> Result<(), ToolError> {
    if let Some((frame_id, text)) = &conversation.unknown {
        save_unknown(ctx, *frame_id, text);
    }
    let Some(events) = ctx.world.events() else {
        return Ok(());
    };
    let index = LabelIndex::build_for(events, &conversation.recognised);
    let resolved = {
        let belief = StateBelief(ctx.state());
        conversation.resolved_in(&index, Some(&belief))
    };
    let Some((script, path)) = resolved else {
        if !conversation.recognised.is_empty() {
            ctx.emit(progress(
                "Dialogue",
                format!(
                    "recognised {:?} but no single script path matches",
                    conversation.recognised
                ),
            ))?;
            // The plan's path didn't run: what it would have done didn't
            // happen.
            if let Some(planned) = conversation.path {
                return Err(ToolError::Replan(format!(
                    "{}[{planned}] did not run: read {:?}",
                    conversation.script.as_deref().unwrap_or("?"),
                    conversation.recognised
                )));
            }
        }
        return Ok(());
    };
    let diverged = conversation
        .path
        .filter(|&planned| planned != path && !same_effects(events, &script, planned, path));
    record_path(ctx, &script, path)?;
    match diverged {
        Some(planned) => Err(ToolError::Replan(format!(
            "{script}: the game took path {path}, not the planned {planned}"
        ))),
        _ => Ok(()),
    }
}

/// Whether two paths of `script` change the same things (they differ only
/// in what they print: the player's gender, the way they faced).
fn same_effects(events: &pokebot_world::events::Events, script: &str, a: usize, b: usize) -> bool {
    let Some(s) = events.script(script) else {
        return false;
    };
    let changes = |i: usize| -> Option<Vec<&Effect>> {
        let p = s.paths.get(i)?;
        Some(
            p.does
                .iter()
                .filter(|e| !matches!(e, Effect::Say { .. }))
                .collect(),
        )
    };
    matches!((changes(a), changes(b)), (Some(x), Some(y)) if x == y)
}

/// Records that path `path` of `script` ran, with its effects, and then
/// the scenes it sets off by itself ([`chained_scenes`]).
pub fn record_path(ctx: &mut ToolContext<'_>, script: &str, path: usize) -> Result<(), ToolError> {
    let world = Arc::clone(&ctx.world);
    let Some(events) = world.events() else {
        return Ok(());
    };
    let map_name = |id: &str| world.name_of(id).map(str::to_owned);
    let mut runs = vec![(script.to_owned(), path)];
    runs.extend(chained_scenes(events, &map_name, script, path));
    for (script, path) in runs {
        let (learned, log) = path_events(
            events,
            &script,
            path,
            ctx.state(),
            &ctx.data,
            world.places(),
            &map_name,
        );
        ctx.emit(progress(
            "Dialogue",
            format!(
                "ran {script}[{path}]: {} events{}",
                learned.len() - 1,
                if log.is_empty() {
                    String::new()
                } else {
                    format!("; {}", log.join("; "))
                }
            ),
        ))?;
        for event in learned {
            ctx.emit(event)?;
        }
    }
    Ok(())
}

/// Writes the frame and the text of an unrecognised conversation under
/// the context's unknown dir.
fn save_unknown(ctx: &mut ToolContext<'_>, frame_id: u64, text: &str) {
    let dir = ctx.unknown_dir.clone();
    let png = dir.join(format!("{frame_id}.png"));
    let txt = dir.join(format!("{frame_id}.txt"));
    let saved = std::fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|()| std::fs::write(&txt, text).map_err(|e| e.to_string()))
        .and_then(|()| match ctx.runtime.last_frame() {
            Some(frame) => pokebot_video::png::save(frame.image(), &png).map_err(|e| e.to_string()),
            None => Ok(()),
        });
    match saved {
        Ok(()) => ctx.info(format!("unknown dialogue saved to {}", png.display())),
        Err(e) => ctx
            .runtime
            .error(format!("unknown dialogue: {}: {e}", png.display())),
    }
    let _ = ctx.runtime.record(
        "UnknownDialogue",
        serde_json::json!({ "frame_id": frame_id, "text": text }),
    );
}

impl LabelIndex {
    /// The index restricted to what `labels` need (cheap: one pass over
    /// the scripts).
    pub fn build_for(events: &pokebot_world::events::Events, labels: &[String]) -> LabelIndex {
        if labels.is_empty() {
            return LabelIndex::default();
        }
        LabelIndex::build(events)
    }
}

/// Runs compiled script `script` (spec §7.2 `Dialogue`), however it
/// starts, and follows it to its end (dialogue, questions, battles, the
/// forced movement of a scene):
///
/// | kind | how it starts |
/// |---|---|
/// | `object` | face the object and press A |
/// | `sign` | face the sign (from the side it is read from) and press A |
/// | `trigger` | step onto the nearest of its tiles |
/// | `map` | enter the map (an `on_frame` scene plays on arrival) |
///
/// A conversation already on screen is followed as the script. A map's
/// scene that already ran (seen and recorded while walking in) is done.
pub struct DialogueTool;

/// Where a script is started from, by its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// Press A facing the object.
    Object { map: String, object: u32 },
    /// Press A facing the sign at `(x, y)`, from `facing`'s side only.
    Sign {
        map: String,
        x: i32,
        y: i32,
        facing: Option<pokebot_state::Direction>,
    },
    /// Step onto one of the tiles.
    Trigger { map: String, tiles: Vec<(i32, i32)> },
    /// Be on the map.
    Map { map: String },
}

/// How `script` is started, from the world data; `None` for scripts no
/// map places (shared helpers).
pub fn start_of(world: &World, script: &str, pose: Option<&PlayerPose>) -> Option<Start> {
    start_of_in(world, script, pose, &|_, _| None)
}

/// [`start_of`], with `shown(map, local_id)` telling which objects are
/// there (seen, `Some(true)`) or gone (`Some(false)`): a script two objects
/// share (Bill, as himself or as a Clefairy) is started at the one there.
pub fn start_of_in(
    world: &World,
    script: &str,
    pose: Option<&PlayerPose>,
    shown: &dyn Fn(&str, u32) -> Option<bool>,
) -> Option<Start> {
    let events = world.events()?;
    let s = events.script(script)?;
    let map = s.map.clone()?;
    let dist = |(x, y): (i32, i32)| {
        pose.filter(|p| p.map == map)
            .map_or(0, |p| (p.x - x).abs() + (p.y - y).abs())
    };
    match s.kind.as_str() {
        "object" => {
            if let Some(object) = s.local_id {
                return Some(Start::Object { map, object });
            }
            let object = events
                .objects
                .iter()
                .filter(|o| o.map == map && o.script.as_deref() == Some(script))
                .min_by_key(|o| {
                    let there = match shown(&map, o.local_id) {
                        Some(true) => 0,
                        None => 1,
                        Some(false) => 2,
                    };
                    let at = (o.x.unwrap_or(0), o.y.unwrap_or(0));
                    (there, dist(at), o.local_id)
                })?;
            Some(Start::Object {
                map,
                object: object.local_id,
            })
        }
        "sign" => {
            let m = world.map(&map)?;
            let sign = m
                .signs
                .iter()
                .filter(|g| g.script.as_deref() == Some(script))
                .min_by_key(|g| (dist((g.x, g.y)), g.x, g.y))?;
            Some(Start::Sign {
                map: map.clone(),
                x: sign.x,
                y: sign.y,
                facing: sign.facing_dir(),
            })
        }
        "trigger" => {
            let mut tiles: Vec<(i32, i32)> = events
                .triggers
                .iter()
                .filter(|t| t.map == map && t.script.as_deref() == Some(script))
                .map(|t| (t.x, t.y))
                .collect();
            tiles.sort_by_key(|&t| (dist(t), t));
            tiles.dedup();
            (!tiles.is_empty()).then_some(Start::Trigger { map, tiles })
        }
        "map" => Some(Start::Map { map }),
        _ => None,
    }
}

/// Walks to where a trigger or a map's scene starts, then follows the
/// scene. The walk ends early when the scene starts on the way (the
/// trigger fired, the map faded in).
pub struct ScriptStep {
    approach: Option<GoStep>,
    pub scene: SceneStep,
}

impl ScriptStep {
    pub fn new(approach: Option<GoStep>, scene: SceneStep) -> Self {
        Self { approach, scene }
    }
}

impl ToolStep for ScriptStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if let Some(go) = &mut self.approach {
            let started = o.dialogue.is_some()
                || o.battle.is_some()
                || o.screen.value == ScreenState::Transition;
            if !started {
                match go.next(ctx) {
                    Decision::Done(_) => {
                        // There: the scene starts by itself.
                        self.approach = None;
                        return Decision::Wait("at the script's start".into());
                    }
                    d => return d,
                }
            }
            self.approach = None;
            self.scene.happened = true;
        }
        self.scene.next(ctx)
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        match &mut self.approach {
            Some(go) => go.on_outcome(action, outcome, ctx),
            None => self.scene.on_outcome(action, outcome, ctx),
        }
    }

    fn expects(&self) -> Expects {
        Expects::DIALOGUE
    }
}

impl Tool for DialogueTool {
    fn name(&self) -> &str {
        "Dialogue"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::RunScript { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::RunScript {
            script,
            path,
            answers,
        } = intent
        else {
            return ToolOutcome::failed("not a RunScript");
        };
        let outer = ctx.running_script.replace(script.clone());
        let result = run_script(ctx, script, *path, answers);
        ctx.running_script = outer;
        result.into()
    }
}

/// [`DialogueTool`]'s work.
fn run_script(
    ctx: &mut ToolContext<'_>,
    script: &str,
    path: Option<usize>,
    answers: &[Answer],
) -> Result<(), ToolError> {
    let pose = ctx.pose();
    let conversation = Conversation::new(
        Arc::clone(&ctx.world),
        Arc::clone(&ctx.data),
        Some(script.to_owned()),
        path,
        answers.to_vec(),
    )
    .near(pose.clone());
    let on_screen = ctx.observation().is_some_and(|o| o.dialogue.is_some());
    let start = {
        let state = ctx.state();
        let shown = |map: &str, id: u32| -> Option<bool> {
            let seen = state
                .view
                .npcs
                .iter()
                .any(|n| n.map == map && n.local_id == Some(id));
            if seen {
                return Some(true);
            }
            if let Some(present) = state.world.npc(map, id).and_then(|n| n.present.value) {
                return Some(present);
            }
            let flag = ctx
                .world
                .map(map)?
                .objects
                .iter()
                .find(|o| o.local_id == id)?
                .flag
                .clone()?;
            state.world.flags.get(&flag)?.value.map(|hidden| !hidden)
        };
        start_of_in(&ctx.world, script, pose.as_ref(), &shown)
    };
    if on_screen || start.is_none() {
        let mut scene = SceneStep::new(conversation);
        ctx.drive(&mut scene)?;
        return finish(ctx, &scene.conversation);
    }
    match start.expect("checked above") {
        Start::Object { map, object } => {
            let mut step = TalkStep::new(ctx, &map, object, answers.to_vec())?.with_scene();
            step.scene.conversation = conversation;
            ctx.drive(&mut step)?;
            finish(ctx, step.conversation())
        }
        Start::Sign { map, x, y, facing } => {
            let mut step = TalkStep::toward(
                ctx,
                &map,
                (x, y),
                facing,
                Some(script.to_owned()),
                answers.to_vec(),
            )
            .with_scene();
            step.scene.conversation = conversation;
            ctx.drive(&mut step)?;
            finish(ctx, step.conversation())
        }
        Start::Trigger { map, tiles } => {
            let (x, y) = tiles[0];
            let go = GoStep::with(
                &NavParts::of(ctx),
                Destination::Tile {
                    map: map.clone(),
                    x,
                    y,
                },
            );
            let mut step = ScriptStep::new(Some(go), SceneStep::new(conversation));
            ctx.drive(&mut step)?;
            let mut conversation = step.scene.conversation;
            // The tile it fired on tells the trigger's paths apart.
            conversation.near = Some(PlayerPose { map, x, y });
            finish(ctx, &conversation)
        }
        Start::Map { map } => {
            if let Some(p) = path {
                let ran = ctx
                    .state()
                    .world
                    .paths_run
                    .iter()
                    .any(|(s, i)| s == script && *i == p);
                if ran {
                    ctx.info(format!("{script}[{p}] already played on arrival"));
                    return Ok(());
                }
            }
            let here = pose.as_ref().is_some_and(|p| p.map == map);
            let approach = (!here).then(|| GoStep::to_map(&NavParts::of(ctx), &map));
            let mut step = ScriptStep::new(approach, SceneStep::new(conversation));
            ctx.drive(&mut step)?;
            finish(ctx, &step.scene.conversation)
        }
    }
}

#[cfg(test)]
mod tests {
    use pokebot_world::events::{Cmp, ScriptPath, VarChange};

    use super::*;

    fn cond_choice(eq: Option<i64>, ne: Option<i64>) -> Condition {
        Condition::Choice {
            choice: "MULTICHOICE_YES_NO".into(),
            cmp: Cmp {
                eq: eq.map(Val::Int),
                ne: ne.map(Val::Int),
                ..Cmp::default()
            },
        }
    }

    fn script(paths: Vec<(Vec<Condition>, Vec<Effect>)>) -> Script {
        Script {
            kind: "object".into(),
            map: None,
            local_id: None,
            paths: paths
                .into_iter()
                .map(|(when, does)| ScriptPath {
                    when,
                    does,
                    opaque: vec![],
                })
                .collect(),
            truncated: false,
        }
    }

    fn say(label: &str) -> Effect {
        Effect::Say { say: label.into() }
    }

    fn world_and_data() -> Option<(Arc<World>, Arc<GameData>)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let world = World::load(&dir).ok()?;
        world.events()?;
        let data = GameData::load(dir.join("gamedata.json")).ok()?;
        Some((Arc::new(world), Arc::new(data)))
    }

    #[test]
    fn scripts_start_where_their_kind_says() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let near = PlayerPose {
            map: "PalletTown".into(),
            x: 13,
            y: 5,
        };
        // A trigger: its tiles, the nearest first.
        assert_eq!(
            start_of(&world, "PalletTown_EventScript_OakTriggerLeft", Some(&near)),
            Some(Start::Trigger {
                map: "PalletTown".into(),
                tiles: vec![(12, 1)]
            })
        );
        let lab = "PalletTown_ProfessorOaksLab";
        let at_exit = PlayerPose {
            map: lab.into(),
            x: 7,
            y: 9,
        };
        match start_of(
            &world,
            "PalletTown_ProfessorOaksLab_EventScript_LeaveStarterSceneTrigger",
            Some(&at_exit),
        ) {
            Some(Start::Trigger { tiles, .. }) => assert_eq!(tiles[0], (7, 8)),
            other => panic!("{other:?}"),
        }
        // A map's entry scene: be on the map.
        assert_eq!(
            start_of(&world, "ViridianCity_Mart_EventScript_ParcelScene", None),
            Some(Start::Map {
                map: "ViridianCity_Mart".into()
            })
        );
        // An object: talk to it.
        assert_eq!(
            start_of(
                &world,
                "PalletTown_ProfessorOaksLab_EventScript_ProfOak",
                None
            ),
            Some(Start::Object {
                map: lab.into(),
                object: 4
            })
        );
        // A sign read from one side only: faced from there.
        let events = world.events().unwrap();
        let (label, sign) = world
            .maps()
            .flat_map(|m| m.signs.iter().map(move |g| (m, g)))
            .filter(|(_, g)| g.facing_dir() == Some(pokebot_state::Direction::Up))
            .filter_map(|(m, g)| {
                let label = g.script.clone()?;
                (events.script(&label)?.kind == "sign"
                    && m.signs.iter().filter(|o| o.script == g.script).count() == 1)
                    .then(|| (label, g.clone()))
            })
            .next()
            .expect("a sign read facing north");
        match start_of(&world, &label, None) {
            Some(Start::Sign { x, y, facing, .. }) => {
                assert_eq!((x, y), (sign.x, sign.y));
                assert_eq!(facing, Some(pokebot_state::Direction::Up));
            }
            other => panic!("{label}: {other:?}"),
        }
    }

    /// `RunScript` of a trigger: walk onto its tile, then follow the scene
    /// it starts (dialogue, then an NPC walking with no text, then more
    /// dialogue) until the player has been in control and the picture at
    /// rest for a while; the walk between the pages is not the end.
    #[test]
    fn a_trigger_is_stepped_onto_and_its_scene_followed_to_the_end() {
        use pokebot_state::{
            DialogueKind, DialogueObservation, FrameMetrics, GameState, Observed, PoseObservation,
            Region,
        };
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let parts = NavParts {
            world: Arc::clone(&world),
            gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
        };
        let go = GoStep::with(
            &parts,
            Destination::Tile {
                map: "PalletTown".into(),
                x: 12,
                y: 1,
            },
        );
        let conversation = Conversation::new(
            Arc::clone(&world),
            Arc::clone(&data),
            Some("PalletTown_EventScript_OakTriggerLeft".into()),
            Some(0),
            Vec::new(),
        );
        let mut step = ScriptStep::new(Some(go), SceneStep::new(conversation));
        let state = GameState::default();
        let obs = |frame: u64, y: i32, text: Option<&str>, changed: u32| {
            let mut o = Observation::bare(
                frame,
                Observed {
                    value: if text.is_some() {
                        ScreenState::Dialogue
                    } else {
                        ScreenState::Overworld
                    },
                    detector: "test".into(),
                },
                FrameMetrics {
                    mean_luma: 100,
                    changed_pixels: changed,
                },
            );
            o.player = Some(PoseObservation {
                pose: PlayerPose {
                    map: "PalletTown".into(),
                    x: 12,
                    y,
                },
                score: 1000,
            });
            o.dialogue = text.map(|t| DialogueObservation {
                kind: DialogueKind::MessageBox,
                region: Region::new(0, 112, 240, 48),
                waiting_for_input: true,
                arrow: None,
                stable_frames: 60,
                text_cells: Vec::new(),
                lines: vec![t.to_owned()],
                help: false,
            });
            o
        };
        let mut events = Vec::new();
        let mut next = |step: &mut ScriptStep, o: &Observation, quiet: u32| {
            step.next(&mut StepContext {
                observation: o,
                state: &state,
                events: &mut events,
                quiet_frames: quiet,
                frame: None,
                learned: &[],
            })
        };
        // One tile below the trigger: step up onto it.
        match next(&mut step, &obs(100, 2, None, 0), 500) {
            // Facing unknown: a turn toward the trigger, or the step.
            Decision::Act(a) => assert!(a.label.contains("to (12, 1)"), "{}", a.label),
            Decision::Wait(r) | Decision::Done(r) | Decision::Fail(r) => panic!("{r}"),
        }
        // On the trigger: the walk is over, the scene starts by itself.
        assert!(matches!(
            next(&mut step, &obs(120, 1, None, 0), 500),
            Decision::Wait(_)
        ));
        assert!(step.approach.is_none());
        // Oak's first line: read on two frames, then advanced.
        let advanced = (130..145).any(|f| {
            matches!(
                next(&mut step, &obs(f, 1, Some("OAK: Hey! Wait!"), 0), 0),
                Decision::Act(a) if a.label == "advance text"
            )
        });
        assert!(advanced);
        // Oak walks up, no text, for 200 frames: the scene goes on.
        for f in (140..340).step_by(4) {
            assert!(
                matches!(next(&mut step, &obs(f, 1, None, 200), 0), Decision::Wait(_)),
                "frame {f}"
            );
        }
        // Then a flower animates now and then while nothing else moves:
        // rest, and the scene ends after SCENE_STILL_FRAMES.
        let mut done = None;
        for f in (340..700).step_by(4) {
            let changed = if f % 16 == 0 { 400 } else { 0 };
            if let Decision::Done(d) = next(&mut step, &obs(f, 1, None, changed), 0) {
                done = Some(f);
                assert!(d.contains("scene over"), "{d}");
                break;
            }
        }
        let done = done.expect("the scene ends");
        assert!(done >= 340 + super::super::scene::SCENE_STILL_FRAMES as u64);
    }

    #[test]
    fn question_branches_read_answers_and_choices() {
        let when = vec![
            Condition::Flag {
                flag: "F".into(),
                is: true,
            },
            cond_choice(Some(0), None),
            Condition::Answer {
                answer: "no".into(),
            },
            cond_choice(None, Some(0)),
        ];
        assert_eq!(question_branches(&when), vec![true, false, false]);
    }

    #[test]
    fn policy_says_yes_to_heals_and_gifts_and_no_to_spending() {
        let nurse = script(vec![
            (
                vec![cond_choice(Some(0), None)],
                vec![say("A"), Effect::Heal { heal: true }],
            ),
            (vec![cond_choice(None, Some(0))], vec![say("A")]),
        ]);
        assert_eq!(policy_answer(Some(&nurse), &[]), Answer::Yes);
        let seller = script(vec![
            (
                vec![cond_choice(Some(0), None)],
                vec![
                    Effect::Take {
                        take: "ITEM_X".into(),
                        count: 1,
                    },
                    Effect::Give {
                        give: "ITEM_Y".into(),
                        count: 1,
                        text: None,
                        find: false,
                    },
                ],
            ),
            (vec![cond_choice(None, Some(0))], vec![]),
        ]);
        assert_eq!(policy_answer(Some(&seller), &[]), Answer::No);
        let chatter = script(vec![
            (vec![cond_choice(Some(0), None)], vec![say("B")]),
            (vec![cond_choice(None, Some(0))], vec![say("C")]),
        ]);
        assert_eq!(policy_answer(Some(&chatter), &[]), Answer::No);
        assert_eq!(policy_answer(None, &[]), Answer::No);
        // A second question, after YES to the first, whose YES gives.
        let two = script(vec![
            (
                vec![cond_choice(Some(0), None), cond_choice(Some(0), None)],
                vec![Effect::Give {
                    give: "ITEM_Y".into(),
                    count: 1,
                    text: None,
                    find: false,
                }],
            ),
            (
                vec![cond_choice(Some(0), None), cond_choice(None, Some(0))],
                vec![],
            ),
        ]);
        assert_eq!(policy_answer(Some(&two), &[true]), Answer::Yes);
        assert_eq!(policy_answer(Some(&two), &[false]), Answer::No);
    }

    #[test]
    fn resolve_path_prefers_the_path_printing_the_most_labels() {
        let s = script(vec![
            (vec![], vec![say("Intro"), say("NoRoom")]),
            (
                vec![],
                vec![
                    say("Intro"),
                    Effect::Give {
                        give: "ITEM_TM39".into(),
                        count: 1,
                        text: Some("Received".into()),
                        find: false,
                    },
                    say("Explain"),
                ],
            ),
            (vec![], vec![say("Later")]),
        ]);
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(resolve_path(&s, &l(&["Intro", "Received"]), &[]), Some(1));
        assert_eq!(resolve_path(&s, &l(&["Intro"]), &[]), Some(0));
        assert_eq!(resolve_path(&s, &l(&["Later"]), &[]), Some(2));
        assert_eq!(resolve_path(&s, &l(&["Unknown"]), &[]), None);
        assert_eq!(resolve_path(&s, &[], &[]), None);
        let one = script(vec![(
            vec![],
            vec![Effect::Var {
                var: "V".into(),
                change: VarChange::default(),
            }],
        )]);
        assert_eq!(resolve_path(&one, &[], &[]), Some(0));
    }

    #[test]
    fn resolve_path_respects_the_answers_given() {
        let s = script(vec![
            (
                vec![cond_choice(Some(0), None)],
                vec![say("Welcome"), say("Healed")],
            ),
            (vec![cond_choice(None, Some(0))], vec![say("Welcome")]),
        ]);
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(resolve_path(&s, &l(&["Welcome"]), &[true]), Some(0));
        assert_eq!(resolve_path(&s, &l(&["Welcome"]), &[false]), Some(1));
        assert_eq!(resolve_path(&s, &l(&["Welcome", "Healed"]), &[]), Some(0));
    }

    /// On the Switch the plan ran Bill's PC before Bill went into the
    /// teleporter: the PC printed its idle message, and the plan's Cell
    /// Separator path must not be recorded (it would set
    /// `FLAG_HELPED_BILL_IN_SEA_COTTAGE`).
    #[test]
    fn the_text_read_outranks_the_planned_path() {
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let script = "Route25_SeaCottage_EventScript_Computer";
        let events = world.events().unwrap();
        let s = events.script(script).unwrap();
        let prints = |i: usize, label: &str| path_labels(&s.paths[i].does).contains(&label);
        let idle = "Route25_SeaCottage_Text_TeleporterIsDisplayed";
        let separator = "Route25_SeaCottage_Text_InitiatedTeleportersCellSeparator";
        let planned = (0..s.paths.len()).find(|&i| prints(i, separator)).unwrap();
        let ran = (0..s.paths.len()).find(|&i| prints(i, idle)).unwrap();
        let mut c = Conversation::new(
            Arc::clone(&world),
            data,
            Some(script.into()),
            Some(planned),
            Vec::new(),
        );
        c.recognised = vec![idle.into()];
        let index = LabelIndex::build_for(events, &c.recognised);
        assert_eq!(c.resolved(&index), Some((script.into(), ran)));
        assert!(!same_effects(events, script, planned, ran));
        // The planned path's own text: the plan stands.
        c.recognised = vec![separator.into()];
        let index = LabelIndex::build_for(events, &c.recognised);
        assert_eq!(c.resolved(&index), Some((script.into(), planned)));
        // Text none of its paths print: nothing of it ran.
        c.recognised = vec!["PalletTown_Text_OakDontGoOut".into()];
        let index = LabelIndex::build_for(events, &c.recognised);
        assert_eq!(c.resolved(&index), None);
    }

    /// Bill's script belongs to him and to the Clefairy he turned into:
    /// it starts at whichever of the two is there (on the Switch the tool
    /// waited for a scene without talking to either).
    #[test]
    fn a_shared_script_starts_at_the_object_that_is_there() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let bill = "Route25_SeaCottage_EventScript_Bill";
        let map = "Route25_SeaCottage";
        let at = |shown: &dyn Fn(&str, u32) -> Option<bool>| match start_of_in(
            &world, bill, None, shown,
        ) {
            Some(Start::Object { object, .. }) => object,
            other => panic!("{other:?}"),
        };
        assert_eq!(at(&|_, id| Some(id == 2)), 2);
        assert_eq!(at(&|_, id| (id == 1).then_some(false)), 2);
        assert_eq!(at(&|_, id| (id == 1).then_some(true)), 1);
        assert!(matches!(
            start_of(&world, bill, None),
            Some(Start::Object { map: m, .. }) if m == map
        ));
    }
}
