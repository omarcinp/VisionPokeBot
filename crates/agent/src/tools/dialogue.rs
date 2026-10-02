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
use pokebot_state::{GameEvent, GameState, MenuObservation, Observation, PlayerPose, ScreenState};
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

/// Whether YES to the first question of `script` is safe to try when no
/// plan asks for it (a recourse trying the other answer): no path taking
/// YES spends money, items or coins, fights, or does something the
/// compiled scripts don't model (a trade is such a special).
pub fn yes_is_safe(script: &Script) -> bool {
    let mut yes = script
        .paths
        .iter()
        .filter(|p| question_branches(&p.when).first() == Some(&true))
        .map(|p| p.does.as_slice())
        .peekable();
    yes.peek().is_some()
        && yes.all(|does| {
            !spends(does)
                && !does.iter().any(|e| {
                    matches!(
                        e,
                        Effect::Battle { .. }
                            | Effect::Wild { .. }
                            | Effect::Coins { .. }
                            | Effect::Other(_)
                    )
                })
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
/// it heals or gives something for free that NO doesn't, NO otherwise.
/// Without a script: NO. (A starter's ball gives the Pokémon before
/// "Give it a nickname?", on both answers: YES there only opens the
/// naming keyboard.)
pub fn policy_answer(script: Option<&Script>, answered: &[bool]) -> Answer {
    let Some(script) = script else {
        return Answer::No;
    };
    let k = answered.len();
    let branches = |yes: bool| -> Vec<&[Effect]> {
        script
            .paths
            .iter()
            .filter_map(|p| {
                let branches = question_branches(&p.when);
                (branches.len() > k && branches[..k] == *answered && branches[k] == yes)
                    .then_some(p.does.as_slice())
            })
            .collect()
    };
    let yes_branches = branches(true);
    if yes_branches.is_empty() {
        return Answer::No;
    }
    if yes_branches.iter().any(|d| spends(d)) {
        return Answer::No;
    }
    if yes_branches.iter().any(|d| gives(d)) && !branches(false).iter().any(|d| gives(d)) {
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
    resolve_path_with(script, labels, answered, belief, false)
}

/// [`resolve_path_in`], knowing whether a battle was fought during the
/// conversation: then a path that fights wins a tie with one that doesn't
/// (the Cerulean grunt's after-battle text is printed both by the path
/// that fights him and by the one for talking to him once beaten; the
/// lower index, the beaten one, was taken and his defeat never tracked).
pub fn resolve_path_with(
    script: &Script,
    labels: &[String],
    answered: &[bool],
    belief: Option<&dyn BeliefView>,
    battled: bool,
) -> Option<usize> {
    let fights = |i: usize| {
        battled
            && script.paths[i]
                .does
                .iter()
                .any(|e| matches!(e, Effect::Battle { .. }))
    };
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
        .max_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| fights(a.0).cmp(&fights(b.0)))
                .then_with(|| b.0.cmp(&a.0))
        })
        .map(|(i, _)| *i)
}

/// A conversation on screen, followed page by page.
/// Up presses that reach the top of any elevator's floor list (Silph Co.:
/// eleven floors and EXIT).
const LIST_TOP_PRESSES: u8 = 14;

pub struct Conversation {
    /// Presses left to reach a planned list row: up to the top, then down.
    list_moves: Option<(u8, u8)>,
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
    /// How many events the running tool had learned when this
    /// conversation began (its own start in `StepContext::learned`).
    learned_at_start: Option<usize>,
    /// A trainer battle was won while it ran: "RED got ¥N for winning!",
    /// which no wild battle prints (Switch: a wild SPEAROW caught on the
    /// way to Cerulean's rival trigger was taken for the rival's battle,
    /// the rival recorded beaten, and the next walk met him at 61/73 HP).
    pub battled: bool,
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
            list_moves: None,
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
            learned_at_start: None,
            battled: false,
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
        // An elevator's floors: a list longer than its window (Switch,
        // Silph Co.: 11F to 1F, six showing, the cursor on the car's floor;
        // the row counted on screen chose 1F, where the car was). The list
        // doesn't wrap: up past its top, then down to the row.
        if let Some(&Answer::ListRow(row)) = self.answers.get(self.answers_used) {
            let (ups, downs) = self.list_moves.get_or_insert((LIST_TOP_PRESSES, row));
            let (button, what) = if *ups > 0 {
                *ups -= 1;
                (
                    Button::Up,
                    format!("choose list row {row} (planned): cursor Up"),
                )
            } else if *downs > 0 {
                *downs -= 1;
                (
                    Button::Down,
                    format!("choose list row {row} (planned): cursor Down"),
                )
            } else {
                self.list_moves = None;
                (Button::A, format!("choose row {row} (planned)"))
            };
            return Decision::Act(Action::new(
                what,
                vec![ControllerCommand::Press(button)],
                Expectation::InputsDone,
                30,
            ));
        }
        if let Some(&Answer::Menu(row)) = self.answers.get(self.answers_used) {
            // A multichoice (an elevator's floors): the plan's row, from
            // wherever the game put the cursor (the car's own floor). A
            // menu too short for it isn't the one planned.
            if row < menu.rows {
                return select(menu, row, &format!("choose row {row} (planned)"));
            }
        }
        if menu.rows > 2 {
            // A menu the plan doesn't need: B.
            return Decision::Act(Action::new(
                "close the menu (B)",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::MenuClosed,
                90,
            ));
        }
        let (yes, why) = match self.answers.get(self.answers_used) {
            Some(Answer::Yes) => (true, "planned"),
            Some(Answer::No) => (false, "planned"),
            _ => (
                policy_answer(self.script_data(), &self.answered) == Answer::Yes,
                "policy",
            ),
        };
        if yes {
            select(menu, 0, &format!("answer YES ({why})"))
        } else {
            select(menu, 1, &format!("answer NO ({why})"))
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
            let path =
                resolve_path_with(data, &self.recognised, &self.answered, belief, self.battled)?;
            return Some((script, path));
        };
        // The plan's path, unless the text read is another path's: the
        // game took the branch it prints, whatever the plan expected (the
        // PC before Bill went into the teleporter shows the idle message,
        // not the Cell Separator's).
        if self.recognised.is_empty() {
            // Pages were shown but none was recognised: a path that
            // changes the world isn't taken on the plan's word (Switch,
            // Vermilion Gym: "Nope! There's only trash here." read without
            // its first line, and the planned "second lock opened" path
            // was recorded: the beams stayed on, and Lt. Surge was walked
            // to eight plans in a row).
            let changes = data.paths.get(planned).is_some_and(|p| {
                use pokebot_world::events::Effect;
                use pokebot_world::gates::{is_local_flag, is_local_var};
                p.does.iter().any(|e| match e {
                    Effect::Say { .. } => false,
                    // The script's own scratch (the trash can's id).
                    Effect::Var { var, .. } => !is_local_var(var),
                    Effect::Set { set: f } | Effect::Clear { clear: f } => !is_local_flag(f),
                    _ => true,
                })
            });
            // A trainer battle fought and won is the path's evidence, its
            // text read or not (Switch: Brock's defeat went unrecorded,
            // FLAG_DEFEATED_BROCK and Pewter's scene var with it, and no
            // plan could leave Pewter).
            let fought = self.battled
                && data.paths.get(planned).is_some_and(|p| {
                    p.does
                        .iter()
                        .any(|e| matches!(e, pokebot_world::events::Effect::Battle { .. }))
                });
            if changes && !fought && !self.pages.is_empty() {
                return None;
            }
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
        let read = resolve_path_with(data, &self.recognised, &self.answered, belief, self.battled)
            .or_else(|| {
                resolve_path_with(data, &self.recognised, &self.answered, None, self.battled)
            });
        match read {
            Some(q) if score(q) > own => Some((script, q)),
            // None of the script's paths prints what was read.
            _ if own == 0 => None,
            _ => Some((script, planned)),
        }
    }
}

/// A trainer battle's win: its prize money (wild battles pay none).
pub(crate) fn trainer_won(e: &GameEvent) -> bool {
    matches!(e, GameEvent::MoneyChanged { delta, reason } if *delta > 0 && reason == "won a battle")
}

impl ToolStep for Conversation {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let start = *self.learned_at_start.get_or_insert(ctx.learned.len());
        self.battled |= ctx
            .learned
            .get(start..)
            .is_some_and(|l| l.iter().any(trainer_won));
        let o = ctx.observation;
        let Some(d) = &o.dialogue else {
            if let Some(menu) = &o.menu {
                // A menu without text (a multichoice after the text closed,
                // or one we don't know): B, unless the plan is answering.
                if self.answers.get(self.answers_used).is_some_and(|a| {
                    matches!(a, Answer::Menu(_) | Answer::ListRow(_)) || menu.rows <= 2
                }) {
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
        // The row confirmed with A answers; the cursor's moves toward it
        // (`…: cursor Down`) don't.
        if outcome != Outcome::Confirmed || action.label.contains(": cursor ") {
            return;
        }
        if action.label.starts_with("answer ") {
            let yes = action.label.starts_with("answer YES");
            self.answered.push(yes);
        } else if !action.label.starts_with("choose row ") {
            return;
        }
        if action.label.contains("(planned)") && self.answers.get(self.answers_used).is_some() {
            self.answers_used += 1;
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
    let world = Arc::clone(&ctx.world);
    let Some(events) = world.events() else {
        return Ok(());
    };
    let index = LabelIndex::build_for(events, &conversation.recognised);
    let ball = found_ball(events, &ctx.data, conversation);
    let resolved = {
        let belief = StateBelief(ctx.state());
        conversation.resolved_in(&index, Some(&belief))
    }
    .or_else(|| ball.as_ref().map(|(s, p, _)| (s.clone(), *p)));
    // What the text read proves, whatever path ran: a condition every
    // path printing it shares (Switch, Vermilion: "The ship set sail." is
    // only printed with the city's scene var at 3, the belief held 1 and
    // walks went on through the ticket check it turns back).
    let shown = conditions_shown(events, &index, &conversation.recognised);
    for event in shown {
        let differs = match &event {
            GameEvent::VarObserved { var, value } => {
                ctx.state().world.vars.get(var).and_then(|k| k.value) != Some(*value)
            }
            GameEvent::FlagObserved { flag, value } => {
                ctx.state().world.flags.get(flag).and_then(|k| k.value) != Some(*value)
            }
            _ => false,
        };
        if differs {
            ctx.emit(progress("Dialogue", format!("the text shows {event:?}")))?;
            ctx.emit(event)?;
        }
    }
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
    if conversation.path == Some(path) {
        let twin = {
            let belief = StateBelief(ctx.state());
            twin_path(
                events,
                &script,
                path,
                &conversation.recognised,
                &conversation.answered,
                &belief,
            )
        };
        if let Some(twin) = twin {
            // What it fought is the battle tool's to record.
            ctx.emit(progress(
                "Dialogue",
                format!(
                    "{script}[{path}] reads the same as [{twin}], and the belief can't \
                     tell which ran: neither is recorded"
                ),
            ))?;
            return Err(ToolError::Replan(format!(
                "{script}: can't tell path {path} from {twin}"
            )));
        }
    }
    let diverged = conversation
        .path
        .filter(|&planned| planned != path && !same_effects(events, &script, planned, path));
    // The ball's item, read only on its "put" page: counted here, once.
    if let Some((_, _, Some(item))) = ball.filter(|(s, p, _)| *s == script && *p == path) {
        let counted = ctx.learned().iter().any(|e| {
            matches!(e, GameEvent::ItemsChanged { item: i, delta, .. } if *i == item && *delta > 0)
        });
        let pocket = ctx
            .data
            .items
            .get(&item)
            .and_then(|i| i.pocket.as_deref())
            .and_then(pokebot_state::Pocket::from_decomp);
        if let (false, Some(pocket)) = (counted, pocket) {
            ctx.emit(GameEvent::ItemsChanged {
                pocket,
                item,
                delta: 1,
                reason: "found".into(),
            })?;
        }
    }
    record_path(ctx, &script, path)?;
    match diverged {
        Some(planned) => Err(ToolError::Replan(format!(
            "{script}: the game took path {path}, not the planned {planned}"
        ))),
        _ => Ok(()),
    }
}

/// The planned path of an item ball (`finditem`) when the conversation
/// shows its item taken, and the item when only its "put" page showed it
/// (not counted yet; a key item's page is the sensor's). The ball prints
/// "RED found X!" and "RED put the X in the Y POCKET.", texts of no
/// script's own, so the item is the path's evidence; unrecorded, the
/// ball's hide flag stayed clear and the taken ball was still believed
/// there. The "found" page plays through with the fanfare and is easily
/// missed (emulator, Mt. Moon 1F: "found a TM09!" never read, "put the
/// TM09 in the TM CASE." read).
fn found_ball(
    events: &pokebot_world::events::Events,
    data: &GameData,
    conversation: &Conversation,
) -> Option<(String, usize, Option<String>)> {
    let script = conversation.script.as_ref()?;
    let path = conversation.path?;
    let item = events
        .script(script)?
        .paths
        .get(path)?
        .does
        .iter()
        .find_map(|e| match e {
            Effect::Give {
                give, find: true, ..
            } => Some(give.clone()),
            _ => None,
        })?;
    let info = data.items.get(&item);
    let put = info.is_some_and(|i| {
        let put = format!(" put the {} in the ", i.name);
        conversation.pages.iter().any(|p| p.contains(&put))
    });
    if conversation.gained_item {
        return Some((script.clone(), path, None));
    }
    let key = info.and_then(|i| i.pocket.as_deref()) == Some("POCKET_KEY_ITEMS");
    put.then(|| (script.clone(), path, (!key).then_some(item)))
}

/// The story state `recognised` proves: the conditions every script path
/// printing all of it shares (a story var at an exact value, a story flag),
/// as observed facts. Scratch vars and flags (`VAR_TEMP_*`, `VAR_0x*`,
/// `FLAG_TEMP_*`) prove nothing past the scene.
pub fn conditions_shown(
    events: &pokebot_world::events::Events,
    index: &LabelIndex,
    recognised: &[String],
) -> Vec<GameEvent> {
    use pokebot_world::gates::{is_local_flag, is_local_var};
    let Some(first) = recognised.first() else {
        return Vec::new();
    };
    let paths: Vec<&pokebot_world::events::ScriptPath> = index
        .paths_for(first)
        .iter()
        .filter_map(|(s, i)| events.script(s)?.paths.get(*i))
        .filter(|p| {
            let labels = path_labels(&p.does);
            recognised.iter().all(|l| labels.contains(&l.as_str()))
        })
        .collect();
    let Some(head) = paths.first() else {
        return Vec::new();
    };
    head.when
        .iter()
        .filter(|c| paths.iter().all(|p| p.when.contains(c)))
        .filter_map(|c| match c {
            Condition::Var { var, cmp } if !is_local_var(var) => {
                let value = cmp.eq.as_ref()?.as_int()?;
                Some(GameEvent::VarObserved {
                    var: var.clone(),
                    value: u16::try_from(value).ok()?,
                })
            }
            Condition::Flag { flag, is } if !is_local_flag(flag) => Some(GameEvent::FlagObserved {
                flag: flag.clone(),
                value: *is,
            }),
            // A trainer's defeat is a flag too: its after-battle words are
            // printed only once beaten (fleet, Rocket Hideout B4F: GRUNT_17
            // retracted with the barrier, "BOSS! I'm sorry I failed you!"
            // read, and the defeat stayed unknown to every plan after).
            Condition::Trainer { trainer, defeated } => Some(GameEvent::FlagObserved {
                flag: trainer.clone(),
                value: *defeated,
            }),
            _ => None,
        })
        .collect()
}

/// Another path of `script` the screen and the belief can't tell from
/// `planned`: it prints every label read, takes the answers given, the
/// belief doesn't rule it out, and it does something else, while the
/// belief doesn't know `planned`'s own conditions hold. Recording
/// `planned` then would take its conditions as facts on the plan's word
/// (fleet, Rocket Hideout B4F: GRUNT_17's script opens the barrier when
/// GRUNT_16 is beaten too, the same text either way; its run recorded
/// GRUNT_16 beaten, never fought, and every walk to GIOVANNI met the
/// barrier, 418 times).
pub fn twin_path(
    events: &pokebot_world::events::Events,
    script: &str,
    planned: usize,
    recognised: &[String],
    answered: &[bool],
    belief: &dyn BeliefView,
) -> Option<usize> {
    let s = events.script(script)?;
    let known = requirement_of(&s.paths.get(planned)?.when)
        .is_some_and(|req| req.iter().all(|q| belief.eval(q) == Truth::True));
    if known {
        return None;
    }
    s.paths.iter().enumerate().find_map(|(i, p)| {
        let branches = question_branches(&p.when);
        let prints = path_labels(&p.does);
        let possible = requirement_of(&p.when)
            .is_none_or(|req| !req.iter().any(|q| belief.eval(q) == Truth::False));
        (i != planned
            && branches.len() >= answered.len()
            && branches[..answered.len()] == *answered
            && possible
            && recognised.iter().all(|l| prints.contains(&l.as_str()))
            && !same_effects(events, script, planned, i))
        .then_some(i)
    })
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
/// Badges the state knows held whose giving script path was never
/// recorded (its text went unread): of the paths giving the badge, the one
/// with the fewest effects is recorded, so the leader's defeat and what it
/// sets follow the badge (Switch: BOULDERBADGE on the Trainer Card, but
/// FLAG_DEFEATED_BROCK and Pewter's scene var unset, and no plan could
/// leave Pewter).
pub fn reconcile_badges(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    let world = Arc::clone(&ctx.world);
    let Some(events) = world.events() else {
        return Ok(());
    };
    for (flag, script, path) in unrecorded_badge_paths(events, &ctx.state().world) {
        ctx.info(format!(
            "{flag} is held but its path never ran: recording {script}[{path}]"
        ));
        record_path_after(ctx, &script, path)?;
    }
    Ok(())
}

/// For each badge held whose giving path isn't recorded: the badge flag
/// and the giving path with the fewest effects ([`reconcile_badges`]).
pub fn unrecorded_badge_paths(
    events: &pokebot_world::events::Events,
    belief: &pokebot_state::WorldBelief,
) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    for n in 1..=8 {
        let flag = format!("FLAG_BADGE{n:02}_GET");
        if belief.flags.get(&flag).and_then(|k| k.value) != Some(true) {
            continue;
        }
        let wanted = flag.as_str();
        let mut givers: Vec<(&str, usize, usize)> = events
            .scripts
            .iter()
            .flat_map(|(name, s)| {
                s.paths.iter().enumerate().filter_map(move |(i, p)| {
                    p.does
                        .iter()
                        .any(|e| matches!(e, Effect::Set { set } if set == wanted))
                        .then_some((name.as_str(), i, p.does.len()))
                })
            })
            .collect();
        let run = &belief.paths_run;
        if givers.is_empty()
            || givers
                .iter()
                .any(|(s, p, _)| run.iter().any(|(rs, rp)| rs == s && rp == p))
        {
            continue;
        }
        givers.sort_by_key(|(s, p, len)| (*len, *s, *p));
        let (script, path, _) = givers[0];
        out.push((flag, script.to_owned(), path));
    }
    out
}

/// The choices the story recorded (`VAR_STARTER_MON`) that the party's
/// species and `also` (the starter the new game picked) imply, tracked
/// when the belief doesn't hold them, so they outlive the Pokémon leaving
/// the party (fleet worker 2: the nugget farm stored CHARIZARD, the
/// starter var went unknown, the Cerulean rival's trigger was taken for
/// another starter's and no plan left Cerulean).
pub fn reconcile_choices(ctx: &mut ToolContext<'_>, also: &[String]) -> Result<(), ToolError> {
    let mut species: Vec<String> = ctx
        .state()
        .party
        .value
        .iter()
        .flatten()
        .filter_map(|m| m.species.value.clone())
        .collect();
    species.extend(also.iter().cloned());
    let world = Arc::clone(&ctx.world);
    let data = Arc::clone(&ctx.data);
    for (var, value) in pokebot_planner::goals::choices_of(&world, &data, &species) {
        if ctx.state().world.var(&var).value.is_none() {
            ctx.info(format!("{var} = {value}: the Pokémon the story gave"));
            ctx.emit(GameEvent::VarTracked { var, value })?;
        }
    }
    Ok(())
}

/// Key items the bag holds whose giving script path was never recorded:
/// the path is recorded, so what it set follows the item (Switch: the
/// LIFT KEY in the bag, FLAG_CAN_USE_ROCKET_HIDEOUT_LIFT unset, and every
/// plan sent the bot to pick up the key that was gone).
pub fn reconcile_key_items(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    let world = Arc::clone(&ctx.world);
    let Some(events) = world.events() else {
        return Ok(());
    };
    for (item, script, path) in unrecorded_key_item_paths(events, ctx.state()) {
        ctx.info(format!(
            "{item} is held but its path never ran: recording {script}[{path}]"
        ));
        record_path_after(ctx, &script, path)?;
    }
    Ok(())
}

/// For each key item held whose giving path isn't recorded, when one
/// script alone gives it: the item and that script's giving path with the
/// fewest effects ([`reconcile_key_items`]).
pub fn unrecorded_key_item_paths(
    events: &pokebot_world::events::Events,
    state: &GameState,
) -> Vec<(String, String, usize)> {
    let Some(held) = state
        .bag
        .pockets
        .get(&pokebot_state::Pocket::KeyItems)
        .and_then(|k| k.value.as_ref())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (item, _) in held.iter().filter(|(_, n)| *n > 0) {
        let mut givers: Vec<(&str, usize, usize)> = events
            .scripts
            .iter()
            .flat_map(|(name, s)| {
                s.paths.iter().enumerate().filter_map(move |(i, p)| {
                    p.does
                        .iter()
                        .any(|e| matches!(e, Effect::Give { give, count, .. } if give == item && *count > 0))
                        .then_some((name.as_str(), i, p.does.len()))
                })
            })
            .collect();
        let run = &state.world.paths_run;
        if givers.is_empty()
            || givers.iter().any(|(s, _, _)| *s != givers[0].0)
            || givers
                .iter()
                .any(|(s, p, _)| run.iter().any(|(rs, rp)| rs == s && rp == p))
        {
            continue;
        }
        givers.sort_by_key(|(s, p, len)| (*len, *s, *p));
        let (script, path, _) = givers[0];
        out.push((item.clone(), script.to_owned(), path));
    }
    out
}

/// [`record_path`] for a path that ran some time ago (a badge or key item
/// held, its path never recorded): where it warped the player then says
/// nothing of where the player is now.
pub fn record_path_after(
    ctx: &mut ToolContext<'_>,
    script: &str,
    path: usize,
) -> Result<(), ToolError> {
    record_path_with(ctx, script, path, false)
}

pub fn record_path(ctx: &mut ToolContext<'_>, script: &str, path: usize) -> Result<(), ToolError> {
    record_path_with(ctx, script, path, true)
}

fn record_path_with(
    ctx: &mut ToolContext<'_>,
    script: &str,
    path: usize,
    now: bool,
) -> Result<(), ToolError> {
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
            if !now && matches!(event, GameEvent::PlayerInferred { .. }) {
                continue;
            }
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

/// Whether object `id` of `map` is there: seen on screen now, else the
/// belief's word on it, else on its hide flag. `None`: nothing known.
pub fn object_shown(world: &World, state: &GameState, map: &str, id: u32) -> Option<bool> {
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
    let flag = world
        .map(map)?
        .objects
        .iter()
        .find(|o| o.local_id == id)?
        .flag
        .clone()?;
    state.world.flags.get(&flag)?.value.map(|hidden| !hidden)
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

/// The trainer a script path fights and must beat (a loss whites out; a
/// battle with a victory text the story goes on from is not one).
fn scripted_battle(world: &World, script: &str, path: usize) -> Option<String> {
    world
        .events()?
        .script(script)?
        .paths
        .get(path)?
        .does
        .iter()
        .find_map(|e| match e {
            Effect::Battle {
                battle,
                victory: None,
                rematch: false,
                ..
            } => Some(battle.clone()),
            _ => None,
        })
}

/// [`DialogueTool`]'s work.
/// The vars a trigger on `tile` of `map` (running `script`) is armed by
/// (`var == v`), each with the other value the script sets it to: what
/// it holds when the trigger, stood on, doesn't fire.
fn disarmed(world: &World, script: &str, map: &str, tile: (i32, i32)) -> Vec<(String, u16)> {
    let Some(events) = world.events() else {
        return Vec::new();
    };
    let Some(s) = events.script(script) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in events
        .triggers
        .iter()
        .filter(|t| t.map == map && (t.x, t.y) == tile && t.script.as_deref() == Some(script))
    {
        for c in &t.when {
            let Condition::Var { var, cmp } = c else {
                continue;
            };
            let Some(armed) = cmp.eq.as_ref().and_then(Val::as_int) else {
                continue;
            };
            let set = s.paths.iter().flat_map(|p| &p.does).find_map(|e| match e {
                Effect::Var { var: v, change } if v == var => change
                    .eq
                    .as_ref()
                    .and_then(Val::as_int)
                    .filter(|n| *n != armed),
                _ => None,
            });
            if let Some(n) = set.and_then(|n| u16::try_from(n).ok()) {
                if !out.iter().any(|(v, _)| v == var) {
                    out.push((var.clone(), n));
                }
            }
        }
    }
    out
}

/// Runs path `path` of `script` within the tool already running: a
/// RunScript whose walk rides an elevator runs the car's panel itself
/// (the toolbox holds RunScript out while it runs: Switch, SILPH CO.
/// 7F's LAPRAS gift failed "RunScript is busy" before a step, the car's
/// panel on its way).
pub(crate) fn run_nested(
    ctx: &mut ToolContext<'_>,
    script: &str,
    path: Option<usize>,
    answers: &[Answer],
) -> Result<(), ToolError> {
    let outer = ctx.running_script.replace(script.to_owned());
    let result = run_script(ctx, script, path, answers);
    ctx.running_script = outer;
    result
}

fn run_script(
    ctx: &mut ToolContext<'_>,
    script: &str,
    path: Option<usize>,
    answers: &[Answer],
) -> Result<(), ToolError> {
    // A trash can of Vermilion Gym's locks: the switches are rolled by
    // the game, so the cans are searched rather than one path run.
    if super::trash_cans::can_of(script).is_some() {
        let open = ctx
            .state()
            .world
            .flags
            .get(super::trash_cans::FLAG)
            .and_then(|k| k.value)
            == Some(true);
        return if open {
            Ok(())
        } else {
            super::trash_cans::solve(ctx)
        };
    }
    // A path that fights a trainer who must be beaten is prepared for
    // like a Beat: heal first when the lead isn't ready (Switch: IVYSAUR
    // met Cerulean's rival at 64/75 through RunScript, never healed, and
    // fainted to his CHARMANDER).
    if let Some(trainer) = path.and_then(|p| scripted_battle(&ctx.world, script, p)) {
        let party = crate::party::Party::from_state(ctx.state());
        if let Some(why) = super::beat::unready(&ctx.data, &party, &trainer) {
            ctx.emit(super::progress(
                "Dialogue",
                format!("healing before {trainer}: {why}"),
            ))?;
            ctx.invoke(&Intent::Heal { center: None }).result?;
        }
    }
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
        let shown = |map: &str, id: u32| object_shown(&ctx.world, ctx.state(), map, id);
        start_of_in(&ctx.world, script, pose.as_ref(), &shown)
    };
    if on_screen || start.is_none() {
        let mut scene = SceneStep::new(conversation);
        ctx.drive(&mut scene)?;
        return finish(ctx, &scene.conversation);
    }
    match start.expect("checked above") {
        Start::Object { map, object } => {
            if let Some(at) = super::lookup::object_tile(&ctx.world, &map, object) {
                super::go::reach_facing(ctx, &map, at)?;
            }
            let mut step = TalkStep::new(ctx, &map, object, answers.to_vec())?.with_scene();
            step.scene.conversation = conversation;
            ctx.drive(&mut step)?;
            finish(ctx, step.conversation())
        }
        Start::Sign { map, x, y, facing } => {
            super::go::reach_facing(ctx, &map, (x, y))?;
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
            if let Err(e) = ctx.drive(&mut step) {
                // Stood on, it didn't fire: the var arming it no longer
                // holds; it holds what the trigger's own script sets
                // (fleet worker 6: the Cerulean rival beaten before a
                // reload the belief kept him unbeaten; every plan walked
                // onto his disarmed trigger, "no scene started").
                let on = ctx
                    .pose()
                    .is_some_and(|p| p.map == map && (p.x, p.y) == (x, y));
                if on && matches!(&e, ToolError::Failed(m) if m.starts_with("no scene started")) {
                    for (var, value) in disarmed(&ctx.world, script, &map, (x, y)) {
                        ctx.emit(GameEvent::VarObserved { var, value })?;
                    }
                }
                return Err(e);
            }
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

    /// Fleet (continue-2): a beaten trainer's after-battle words show its
    /// defeat, whatever the belief held.
    #[test]
    fn a_trainers_after_battle_words_show_its_defeat() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let read = ["RocketHideout_B4F_Text_Grunt3PostBattle".to_owned()];
        let index = LabelIndex::build_for(events, &read);
        let shown = conditions_shown(events, &index, &read);
        assert!(
            shown.contains(&GameEvent::FlagObserved {
                flag: "TRAINER_TEAM_ROCKET_GRUNT_17".into(),
                value: true,
            }),
            "{shown:?}"
        );
    }

    /// Fleet (continue-2), Rocket Hideout B4F: GRUNT_17's script prints
    /// the same pages whether GRUNT_16 is beaten (path 0, the barrier
    /// opens) or not (path 1). With GRUNT_16 unknown the two can't be
    /// told apart, and path 0 isn't taken on the plan's word; known
    /// beaten, it is.
    #[test]
    fn a_planned_path_reading_the_same_as_another_is_not_taken_on_trust() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let script = "RocketHideout_B4F_EventScript_Grunt3";
        let read = [
            "RocketHideout_B4F_Text_Grunt3Intro".to_owned(),
            "RocketHideout_B4F_Text_Grunt3Defeat".to_owned(),
        ];
        let mut state = GameState::default();
        let twin = |state: &GameState, planned| {
            twin_path(events, script, planned, &read, &[], &StateBelief(state))
        };
        assert_eq!(twin(&state, 0), Some(1));
        state.world.flags.insert(
            "TRAINER_TEAM_ROCKET_GRUNT_16".into(),
            pokebot_state::Knowledge::tracked(true, None),
        );
        assert_eq!(twin(&state, 0), None);
        // Unbeaten, the planned path 1 is the only one left.
        state.world.flags.insert(
            "TRAINER_TEAM_ROCKET_GRUNT_16".into(),
            pokebot_state::Knowledge::tracked(false, None),
        );
        assert_eq!(twin(&state, 1), None);
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
            data: None,
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

    /// The Cerulean grunt's after-battle text: the path that fought him
    /// when a battle was fought, else the one for him already beaten.
    #[test]
    fn a_battle_in_the_conversation_picks_the_path_that_fights() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else {
            return;
        };
        let Some(script) = world
            .events()
            .and_then(|e| e.script("CeruleanCity_EventScript_Grunt"))
        else {
            return;
        };
        let labels = vec![
            "CeruleanCity_Text_OkayIllReturnStolenTM".to_owned(),
            "CeruleanCity_Text_RecoveredTM28FromGrunt".to_owned(),
        ];
        let without = resolve_path_with(script, &labels, &[], None, false).unwrap();
        let with = resolve_path_with(script, &labels, &[], None, true).unwrap();
        assert!(!script.paths[without]
            .does
            .iter()
            .any(|e| matches!(e, Effect::Battle { .. })));
        assert!(script.paths[with].does.iter().any(|e| matches!(
            e,
            Effect::Battle { battle, .. } if battle == "TRAINER_TEAM_ROCKET_GRUNT_5"
        )));
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
        // "Give it a nickname?" after the gift: both answers give the
        // Pokémon (on the emulator YES opened the naming keyboard).
        let mon = || Effect::GiveMon {
            givemon: Val::Int(1),
            level: Val::Int(5),
        };
        let ball = script(vec![
            (
                vec![cond_choice(Some(0), None), cond_choice(Some(0), None)],
                vec![mon(), say("NAMING")],
            ),
            (
                vec![cond_choice(Some(0), None), cond_choice(None, Some(0))],
                vec![mon()],
            ),
            (vec![cond_choice(None, Some(0))], vec![]),
        ]);
        assert_eq!(policy_answer(Some(&ball), &[]), Answer::Yes);
        assert_eq!(policy_answer(Some(&ball), &[true]), Answer::No);
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

    /// Switch, Vermilion Gym: the page was read without its first line
    /// ("There's only trash here.") and nothing was recognised; the plan's
    /// "second lock opened" path (the beams off) must not be recorded on
    /// its word. A path that only prints still is.
    #[test]
    fn unrecognised_text_does_not_prove_a_planned_path_that_changes_things() {
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let script = "VermilionCity_Gym_EventScript_TrashCan10";
        let events = world.events().unwrap();
        let s = events.script(script).unwrap();
        let prints = |i: usize, label: &str| path_labels(&s.paths[i].does).contains(&label);
        let second = (0..s.paths.len())
            .find(|&i| prints(i, "VermilionCity_Gym_Text_SecondLockOpened"))
            .unwrap();
        let nope = (0..s.paths.len())
            .find(|&i| {
                prints(i, "VermilionCity_Gym_Text_NopeOnlyTrashHere")
                    && !s.paths[i]
                        .does
                        .iter()
                        .any(|e| matches!(e, pokebot_world::events::Effect::Metatile { .. }))
            })
            .unwrap();
        let conversation = |path| {
            let mut c = Conversation::new(
                Arc::clone(&world),
                Arc::clone(&data),
                Some(script.into()),
                Some(path),
                Vec::new(),
            );
            c.pages = vec!["There’s only trash here.".into()];
            c
        };
        let index = LabelIndex::build_for(events, &[]);
        assert_eq!(conversation(second).resolved(&index), None);
        assert_eq!(
            conversation(nope).resolved(&index),
            Some((script.into(), nope))
        );
        // A battle fought and won is the path's evidence (Switch: Brock's
        // defeat, its text unread, went unrecorded).
        let brock = "PewterCity_Gym_EventScript_Brock";
        let b = events.script(brock).unwrap();
        let fights = (0..b.paths.len())
            .find(|&i| {
                b.paths[i]
                    .does
                    .iter()
                    .any(|e| matches!(e, pokebot_world::events::Effect::Battle { .. }))
            })
            .unwrap();
        let mut c = Conversation::new(
            Arc::clone(&world),
            Arc::clone(&data),
            Some(brock.into()),
            Some(fights),
            Vec::new(),
        );
        c.pages = vec!["…".into()];
        assert_eq!(c.resolved(&index), None, "no battle yet");
        // A wild battle on the way is no trainer's (Switch: a SPEAROW
        // caught before Cerulean's rival trigger was taken for the rival):
        // only a trainer's prize money is.
        let o = pokebot_state::Observation::bare(
            1,
            pokebot_state::Observed {
                value: pokebot_state::ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        let state = pokebot_state::GameState::default();
        let step = |c: &mut Conversation, learned: &[GameEvent]| {
            let mut events = Vec::new();
            c.next(&mut StepContext {
                observation: &o,
                state: &state,
                events: &mut events,
                quiet_frames: 0,
                frame: None,
                learned,
            });
        };
        let wild = [
            GameEvent::BattleStarted,
            GameEvent::SpeciesCaught {
                species: "SPECIES_SPEAROW".into(),
            },
            GameEvent::BattleEnded,
        ];
        step(&mut c, &[]);
        step(&mut c, &wild);
        assert!(!c.battled, "a wild battle");
        assert_eq!(c.resolved(&index), None, "a wild battle is not Brock's");
        let mut won = wild.to_vec();
        won.push(GameEvent::MoneyChanged {
            delta: 1386,
            reason: "won a battle".into(),
        });
        step(&mut c, &won);
        assert!(c.battled);
        assert_eq!(c.resolved(&index), Some((brock.into(), fights)));
    }

    /// An item ball prints only "RED found X!" and "RED put the X in the
    /// Y POCKET.": either is its path's evidence, so the ball's hide flag
    /// is recorded. Neither (a full bag), nothing recorded; a gift's text
    /// is its own.
    #[test]
    fn a_ball_whose_item_was_found_ran_its_path() {
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let ball = "ViridianForest_EventScript_ItemAntidote";
        let mut c = Conversation::new(
            Arc::clone(&world),
            Arc::clone(&data),
            Some(ball.into()),
            Some(0),
            vec![],
        );
        assert_eq!(found_ball(events, &data, &c), None);
        // Only the "put" page read: the item is still to count.
        c.pages = vec!["GOLD put the ANTIDOTE in the ITEMS POCKET.".into()];
        assert_eq!(
            found_ball(events, &data, &c),
            Some((ball.into(), 0, Some("ITEM_ANTIDOTE".into())))
        );
        c.gained_item = true;
        assert_eq!(found_ball(events, &data, &c), Some((ball.into(), 0, None)));
        let (learned, _) = super::super::effects::path_events(
            events,
            ball,
            0,
            &GameState::default(),
            &data,
            None,
            &|_| None,
        );
        assert!(learned.contains(&GameEvent::FlagTracked {
            flag: "FLAG_HIDE_VIRIDIAN_FOREST_ANTIDOTE".into(),
            value: true
        }));
        let mut gift = Conversation::new(
            Arc::clone(&world),
            Arc::clone(&data),
            Some("Route1_EventScript_MartClerk".into()),
            Some(2),
            vec![],
        );
        gift.gained_item = true;
        assert_eq!(found_ball(events, &data, &gift), None);
    }

    /// A RunScript that fights a trainer who must be beaten heals first
    /// like a Beat (Switch: the Cerulean rival met at 64/75); Oak's lab
    /// rival, whose loss the story goes on from, isn't one.
    #[test]
    fn a_scripted_must_win_battle_is_found() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let fights = |script: &str| -> Vec<String> {
            let n = events.script(script).map_or(0, |s| s.paths.len());
            (0..n)
                .filter_map(|p| scripted_battle(&world, script, p))
                .collect()
        };
        let rival = fights("CeruleanCity_EventScript_RivalTriggerLeft");
        assert!(
            rival.contains(&"TRAINER_RIVAL_CERULEAN_CHARMANDER".to_owned()),
            "{rival:?}"
        );
        assert!(
            fights("PewterCity_Gym_EventScript_Brock").contains(&"TRAINER_LEADER_BROCK".to_owned())
        );
        let lab = fights("PalletTown_ProfessorOaksLab_EventScript_RivalBattleTriggerMid");
        assert!(lab.is_empty(), "{lab:?}");
    }

    /// Switch, Vermilion: the belief held the city's scene var at 1 (the
    /// S.S. Anne's departure missed), so the ticket check looked passable,
    /// and "The ship set sail." turned the walk back for half an hour. That
    /// text is printed only with the var at 3: reading it proves it (and
    /// no scratch var); nothing read proves nothing.
    #[test]
    fn a_text_proves_what_every_path_printing_it_requires() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let said = vec!["VermilionCity_Text_TheShipSetSail".to_owned()];
        let index = LabelIndex::build_for(events, &said);
        let shown = conditions_shown(events, &index, &said);
        assert!(
            shown.contains(&GameEvent::VarObserved {
                var: "VAR_MAP_SCENE_VERMILION_CITY".into(),
                value: 3,
            }),
            "{shown:?}"
        );
        assert!(!shown.iter().any(|e| matches!(e,
            GameEvent::VarObserved { var, .. } if var.starts_with("VAR_TEMP") || var.starts_with("VAR_0x"))));
        let nothing = conditions_shown(events, &index, &[]);
        assert!(nothing.is_empty());
    }

    /// Fleet worker 4: Brock beaten with his opening pages unrecognised,
    /// the badge held but his path unrecorded, and Pewter's gym-guide gate
    /// shut for the rest of the cycle. The badge names the path to record;
    /// once recorded, nothing is left to do.
    #[test]
    fn a_badge_held_names_its_unrecorded_giving_path() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let mut belief = pokebot_state::WorldBelief::default();
        assert!(unrecorded_badge_paths(events, &belief).is_empty());
        belief.flags.insert(
            "FLAG_BADGE01_GET".into(),
            pokebot_state::Knowledge::observed(true, 1),
        );
        let found = unrecorded_badge_paths(events, &belief);
        assert_eq!(found.len(), 1, "{found:?}");
        let (flag, script, path) = &found[0];
        assert_eq!(flag, "FLAG_BADGE01_GET");
        assert_eq!(script, "PewterCity_Gym_EventScript_Brock");
        belief.record_path(script, *path);
        assert!(unrecorded_badge_paths(events, &belief).is_empty());
    }

    /// Fleet worker 6: the Cerulean rival beaten, the belief reloaded
    /// with him unbeaten; his trigger, stood on, didn't fire. Its arming
    /// var holds what his script sets, 1.
    #[test]
    fn a_trigger_that_does_not_fire_is_disarmed() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let script = "CeruleanCity_EventScript_RivalTriggerLeft";
        let t = events
            .triggers
            .iter()
            .find(|t| t.script.as_deref() == Some(script))
            .unwrap();
        let found = disarmed(&world, script, "CeruleanCity", (t.x, t.y));
        assert_eq!(
            found,
            vec![("VAR_MAP_SCENE_CERULEAN_CITY_RIVAL".to_string(), 1)]
        );
        assert!(disarmed(&world, script, "CeruleanCity", (0, 0)).is_empty());
    }

    /// The Switch: the LIFT KEY in the bag, its script unrecorded. The
    /// key names the Rocket Hideout's Lift Key path; once recorded,
    /// nothing is left to do.
    #[test]
    fn a_key_item_held_names_its_unrecorded_giving_path() {
        let Some((world, _)) = world_and_data() else {
            return;
        };
        let events = world.events().unwrap();
        let mut state = GameState::default();
        assert!(unrecorded_key_item_paths(events, &state).is_empty());
        state.bag.pockets.insert(
            pokebot_state::Pocket::KeyItems,
            pokebot_state::Knowledge::observed(vec![("ITEM_LIFT_KEY".into(), 1)], 1),
        );
        let found = unrecorded_key_item_paths(events, &state);
        assert_eq!(found.len(), 1, "{found:?}");
        let (item, script, path) = &found[0];
        assert_eq!(item, "ITEM_LIFT_KEY");
        assert_eq!(script, "RocketHideout_B4F_EventScript_LiftKey");
        let sets = &events.script(script).unwrap().paths[*path].does;
        assert!(sets.iter().any(|e| matches!(e,
            Effect::Set { set } if set == "FLAG_CAN_USE_ROCKET_HIDEOUT_LIFT")));
        state.world.record_path(script, *path);
        assert!(unrecorded_key_item_paths(events, &state).is_empty());
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

    /// Rocket Hideout's lift: "Which floor do you want?" over a menu of
    /// B1F, B2F, B4F and EXIT, the cursor on the car's own floor. The
    /// plan's row (B4F, 2) is walked to from wherever the cursor is and
    /// confirmed with A; only the A counts as the answer.
    #[test]
    fn a_planned_menu_row_moves_the_cursor_there_and_presses_a() {
        use pokebot_state::{Observed, Region};
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let answers = super::super::parse_answers(&[
            "MULTICHOICE_ROCKET_HIDEOUT_ELEVATOR=2".into(),
            "MULTICHOICE_YES_NO=0".into(),
            "yes".into(),
        ]);
        // An elevator's floors are a list menu: answered by row in the list.
        assert_eq!(answers, vec![Answer::ListRow(2), Answer::Yes]);
        let mut c = Conversation::new(
            world,
            data,
            Some("RocketHideout_Elevator_EventScript_FloorSelect".into()),
            None,
            vec![Answer::Menu(2), Answer::Yes],
        );
        let menu = |cursor_row: u8| MenuObservation {
            window: Region::new(8, 8, 64, 68),
            rows: 4,
            cursor_row,
            cursor_y: 12 + 16 * u32::from(cursor_row),
        };
        let act = |d: Decision| match d {
            Decision::Act(a) => a,
            _ => panic!("expected an act"),
        };
        let observation = Observation::bare(
            1,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        let state = GameState::default();
        let mut events = Vec::new();
        let mut ctx = StepContext {
            observation: &observation,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        };
        // The car is on B1F: down twice, then A.
        let down = act(c.answer(&menu(0)));
        assert_eq!(down.commands, vec![ControllerCommand::Press(Button::Down)]);
        c.on_outcome(&down, Outcome::Confirmed, &mut ctx);
        assert_eq!(c.answers_used, 0, "a cursor move is no answer");
        let down = act(c.answer(&menu(1)));
        assert_eq!(down.commands, vec![ControllerCommand::Press(Button::Down)]);
        let press = act(c.answer(&menu(2)));
        assert_eq!(press.commands, vec![ControllerCommand::Press(Button::A)]);
        assert_eq!(press.expect, Expectation::MenuClosed);
        c.on_outcome(&press, Outcome::Confirmed, &mut ctx);
        assert_eq!(c.answers_used, 1);
        assert!(c.answered.is_empty(), "a floor is no YES/NO");
        // From below (EXIT), up.
        let mut c2 = Conversation::new(
            Arc::clone(&c.world),
            Arc::clone(&c.data),
            None,
            None,
            vec![Answer::Menu(2)],
        );
        let up = act(c2.answer(&menu(3)));
        assert_eq!(up.commands, vec![ControllerCommand::Press(Button::Up)]);
        // The next planned answer, YES, on a YES/NO box.
        let yes_no = MenuObservation { rows: 2, ..menu(0) };
        let press = act(c.answer(&yes_no));
        assert_eq!(press.commands, vec![ControllerCommand::Press(Button::A)]);
        assert!(press.label.starts_with("answer YES (planned)"));
        // Without a plan, a tall menu is still closed with B.
        let mut c3 = Conversation::new(
            Arc::clone(&c.world),
            Arc::clone(&c.data),
            None,
            None,
            Vec::new(),
        );
        let close = act(c3.answer(&menu(0)));
        assert_eq!(close.commands, vec![ControllerCommand::Press(Button::B)]);
    }

    /// Switch, Silph Co.'s lift: eleven floors and EXIT, six rows showing,
    /// the cursor on the car's floor. The row counted on screen chose 1F
    /// (the car's own). A list row goes up past the top, then down to the
    /// row, then A; only the A answers.
    #[test]
    fn an_elevator_list_row_is_counted_from_the_top() {
        use pokebot_state::{Observed, Region};
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let mut c = Conversation::new(world, data, None, None, vec![Answer::ListRow(0)]);
        let menu = MenuObservation {
            window: Region::new(0, 0, 48, 112),
            rows: 6,
            cursor_row: 4,
            cursor_y: 80,
        };
        let o = Observation::bare(
            1,
            Observed {
                value: ScreenState::Dialogue,
                detector: "test".into(),
            },
            Default::default(),
        );
        let state = GameState::default();
        let mut events = Vec::new();
        let mut ctx = StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        };
        let act = |d: Decision| match d {
            Decision::Act(a) => a,
            _ => panic!("expected an action"),
        };
        let pressed = |a: &Action| match a.commands.as_slice() {
            [ControllerCommand::Press(b)] => *b,
            other => panic!("{other:?}"),
        };
        let mut presses = Vec::new();
        loop {
            let a = act(c.answer(&menu));
            let b = pressed(&a);
            c.on_outcome(&a, Outcome::Confirmed, &mut ctx);
            presses.push(b);
            if b == Button::A {
                break;
            }
            assert_eq!(c.answers_used, 0, "a cursor move is no answer");
        }
        assert_eq!(presses.len(), usize::from(LIST_TOP_PRESSES) + 1);
        assert!(presses[..presses.len() - 1]
            .iter()
            .all(|b| *b == Button::Up));
        assert_eq!(c.answers_used, 1);
        // Row 2: up to the top, two down, A.
        let mut c = Conversation::new(
            Arc::clone(&c.world),
            Arc::clone(&c.data),
            None,
            None,
            vec![Answer::ListRow(2)],
        );
        let presses: Vec<Button> = (0..usize::from(LIST_TOP_PRESSES) + 3)
            .map(|_| pressed(&act(c.answer(&menu))))
            .collect();
        let n = presses.len();
        assert_eq!(&presses[n - 3..], &[Button::Down, Button::Down, Button::A]);
    }

    /// A planned NO starts with the cursor on YES: the move down to NO is
    /// not the answer (it was counted, and the A then answered NO again
    /// by policy, two answers for one question).
    #[test]
    fn a_planned_no_is_answered_once() {
        use pokebot_state::{Observed, Region};
        let Some((world, data)) = world_and_data() else {
            return;
        };
        let mut c = Conversation::new(world, data, None, None, vec![Answer::No]);
        let menu = |cursor_row: u8| MenuObservation {
            window: Region::new(8, 8, 40, 36),
            rows: 2,
            cursor_row,
            cursor_y: 12 + 16 * u32::from(cursor_row),
        };
        let observation = Observation::bare(
            1,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        let state = GameState::default();
        let mut events = Vec::new();
        let mut ctx = StepContext {
            observation: &observation,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        };
        for cursor in [0, 1] {
            let Decision::Act(a) = c.answer(&menu(cursor)) else {
                panic!("expected an act");
            };
            c.on_outcome(&a, Outcome::Confirmed, &mut ctx);
        }
        assert_eq!(c.answered, vec![false]);
        assert_eq!(c.answers_used, 1);
    }
}
