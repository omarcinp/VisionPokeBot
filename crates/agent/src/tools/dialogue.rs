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
use pokebot_state::{GameEvent, MenuObservation, Observation};
use pokebot_world::dialogue::Dialogue;
use pokebot_world::events::{Condition, Effect, Script, Val};
use pokebot_world::World;

use super::effects::{path_events, path_labels, LabelIndex};
use super::talk::TalkStep;
use super::{
    progress, Answer, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep, SETTLE_FRAMES,
};
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
    let consistent: Vec<(usize, usize)> = script
        .paths
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            let branches = question_branches(&p.when);
            branches.len() >= answered.len() && branches[..answered.len()] == *answered
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
        }
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
        match index.scripts_for(&self.recognised).as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// The (script, path) that ran, if it can be told.
    pub fn resolved(&self, index: &LabelIndex) -> Option<(String, usize)> {
        let script = self.resolved_script(index)?;
        if let Some(p) = self.path {
            return Some((script, p));
        }
        let data = self.world.events()?.script(&script)?;
        let path = resolve_path(data, &self.recognised, &self.answered)?;
        Some((script, path))
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
                ctx.events.extend(tracked);
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
    let Some((script, path)) = conversation.resolved(&index) else {
        if !conversation.recognised.is_empty() {
            ctx.emit(progress(
                "Dialogue",
                format!(
                    "recognised {:?} but no single script path matches",
                    conversation.recognised
                ),
            ))?;
        }
        return Ok(());
    };
    let world = Arc::clone(&ctx.world);
    let map_name = |id: &str| world.name_of(id).map(str::to_owned);
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

/// Follows the conversation on screen as `script` (spec §7.2 `Dialogue`).
pub struct DialogueTool;

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
        let conversation = Conversation::new(
            Arc::clone(&ctx.world),
            Arc::clone(&ctx.data),
            Some(script.clone()),
            *path,
            answers.clone(),
        );
        // An object's script starts by talking to it: approach and press A
        // unless its conversation is already on screen.
        let on_screen = ctx.observation().is_some_and(|o| o.dialogue.is_some());
        let object = ctx
            .world
            .events()
            .and_then(|e| e.script(script))
            .filter(|s| s.kind == "object")
            .and_then(|s| Some((s.map.clone()?, s.local_id?)));
        if let (false, Some((map, object))) = (on_screen, object) {
            let mut step = match TalkStep::new(ctx, &map, object, answers.clone()) {
                Ok(step) => step,
                Err(e) => return Err::<(), _>(e).into(),
            };
            step.conversation = conversation;
            return ctx
                .drive(&mut step)
                .and_then(|_| finish(ctx, &step.conversation))
                .into();
        }
        let mut conversation = conversation;
        ctx.drive(&mut conversation)
            .and_then(|_| finish(ctx, &conversation))
            .into()
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
}
