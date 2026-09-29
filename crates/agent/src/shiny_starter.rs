//! Shiny starter hunt, the doing half: one attempt from a save made in
//! Oak's lab, facing the chosen Poké Ball.
//!
//! An attempt soft-resets, presses A on the title screen `title` frames
//! later (that picks the RNG seed), continues, talks to the ball, answers
//! YES and closes "This POKéMON is really quite energetic!" with an A
//! exactly `last` frames after the title press (the starter is made right
//! after it). All of that is **one** controller [`ControllerCommand::Sequence`]
//! built from frame offsets, so its timing never depends on when the bot
//! sees a frame: on the ESP32 it plays from the device's own 1 ms clock,
//! on the stepped emulator to the frame. The rest is closed-loop: decline
//! the nickname, let the rival take his ball, open the starter's summary and
//! read its nature, stats and shiny star.
//!
//! The offsets between screens (title, main menu, the recap, the question)
//! are measured once by a closed-loop **scout** attempt ([`Scouted`]); the
//! timed script then presses every button a margin after the scouted time.
//!
//! Presses the script uses, and why they're safe where they may land:
//! - Select skips the intro (`Task_CallIntroCallback`), and does nothing on
//!   the title screen. B skips the title's opening animation to its RUN
//!   state and does nothing in RUN. They alternate, never held together
//!   (B+Select held is the Berry Fix combo on the cartridge).
//! - Only A locks the seed (RUN: `JOY_NEW(A_BUTTON | START_BUTTON)`).
//! - B during the "Previously on your quest…" recap skips it, and does
//!   nothing on the field.

use std::time::{Duration, Instant};

use pokebot_controller::FrameRate;
use pokebot_core::{Button, ButtonSet, ControllerCommand, TimedInput};
use pokebot_gamedata::rng::Reading;
use pokebot_gamedata::training::nature_named;
use pokebot_state::{Observation, ScreenState, ShinyReading, SummaryPage};
use serde::{Deserialize, Serialize};

use crate::new_game::{advance_or_wait, select};
use crate::shiny_plan::Timing;
use crate::{Action, Decision, Expectation, Outcome, Task, TaskContext};

/// Hard reset: frames from the intro showing to the first Select.
const INTRO_SKIP: u32 = 2;
/// Frames a scripted press holds its button(s).
const PRESS: u32 = 5;
/// Frames the soft-reset chord is held.
const RESET_HOLD: u32 = 15;
/// Frames between the mash taps (Select/B before the title, B in the recap).
const MASH_EVERY: u32 = 10;
/// Frames every scripted press waits past the scouted time it may land.
pub const MARGIN: u32 = 30;
/// Frames after the title screen shows before its RUN state is certain.
const TITLE_SETTLE: u32 = 40;
/// The ESP32 queues at most 256 timeline steps.
const MAX_STEPS: usize = 250;
/// Quiet frames on the field that count as having control.
const CONTROL_FRAMES: u32 = 10;
/// Frames standing on the field before an attempt that returns there ends.
pub const FIELD_IDLE: u32 = 60;
/// Frames a summary page must read the same before it's trusted.
const SUMMARY_STABLE: u64 = 15;

fn buttons(list: &[Button]) -> ButtonSet {
    list.iter().copied().collect()
}

/// How an attempt starts over.
///
/// The in-game soft reset (A+B+Start+Select) is fast, but the seed it leads
/// to depends on what the game did before the reset (measured on mGBA: the
/// same timing after different attempts gives different seeds; RNG guides
/// ask for a hard boot for the same reason). A power cycle (the emulator's
/// power switch; on a Switch, restarting the software) makes the seed a
/// function of the timing alone: the attempt waits for the Game Freak
/// intro, and its first Select skips it at a frame of its own script.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum ResetKind {
    /// The in-game soft reset: blind timings only.
    #[default]
    Soft,
    /// Power cycled before the attempt; timed from the intro.
    Hard,
}

/// Frame offsets measured by the scout, each from the press that led to
/// the screen: the reset (soft: the chord; hard: the intro showing) to the
/// title screen, the title press to the
/// main menu, CONTINUE to control on the field, and YES to "…really quite
/// energetic!" waiting for A. `dialogue` lists, from talking to the ball and
/// then from each A on its text, the frames until the next page waits for
/// A; the last is to the YES/NO question ("Ah! CHARMANDER is your choice…"
/// is a page before it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scouted {
    #[serde(default)]
    pub reset: ResetKind,
    pub title_seen: u32,
    pub menu_after_title: u32,
    pub control_after_continue: u32,
    pub dialogue: Vec<u32>,
    pub ready_after_yes: u32,
}

impl Scouted {
    /// Earliest title press (the title screen in RUN).
    pub fn title_min(&self) -> u32 {
        self.title_seen + TITLE_SETTLE
    }

    /// Latest title press: the title restarts after 2700 frames in RUN.
    pub fn title_max(&self) -> u32 {
        self.title_min() + 2400
    }

    /// Earliest last press after the title press.
    pub fn last_min(&self) -> u32 {
        self.menu_after_title
            + self.control_after_continue
            + self.dialogue.iter().sum::<u32>()
            + self.ready_after_yes
            + (3 + self.dialogue.len() as u32) * MARGIN
    }

    /// The whole attempt as `(frame, buttons, hold)` presses.
    fn presses(&self, t: Timing) -> Result<Vec<(u32, ButtonSet, u32)>, String> {
        if t.title < self.title_min() || t.last < self.last_min() {
            return Err(format!(
                "timing {t:?} is too early (title ≥ {}, last ≥ {})",
                self.title_min(),
                self.last_min()
            ));
        }
        let (mut out, mut at) = match self.reset {
            ResetKind::Soft => (
                vec![(
                    0,
                    buttons(&[Button::A, Button::B, Button::Start, Button::Select]),
                    RESET_HOLD,
                )],
                RESET_HOLD + 2 * PRESS,
            ),
            // The intro shows: skip it right away.
            ResetKind::Hard => (Vec::new(), INTRO_SKIP),
        };
        let mash_end = (self.title_seen + MARGIN).min(t.title - 2 * MASH_EVERY);
        let mut select = true;
        while at < mash_end {
            let b = if select { Button::Select } else { Button::B };
            out.push((at, buttons(&[b]), PRESS));
            select = !select;
            at += MASH_EVERY;
        }
        out.push((t.title, buttons(&[Button::A]), PRESS));
        let continue_at = t.title + self.menu_after_title + MARGIN;
        out.push((continue_at, buttons(&[Button::A]), PRESS));
        let talk_at = continue_at + self.control_after_continue + MARGIN;
        // The main menu fades out first; a B on it would go back to the title.
        let mut at = continue_at + 2 * MARGIN;
        while at + MASH_EVERY < talk_at {
            out.push((at, buttons(&[Button::B]), PRESS));
            at += MASH_EVERY;
        }
        out.push((talk_at, buttons(&[Button::A]), PRESS));
        // Each page, then YES (the question's cursor starts on it).
        let mut at = talk_at;
        for delay in &self.dialogue {
            at += delay + MARGIN;
            out.push((at, buttons(&[Button::A]), PRESS));
        }
        out.push((t.title + t.last, buttons(&[Button::A]), PRESS));
        Ok(out)
    }

    /// The attempt as one timeline (press, then released until the next).
    pub fn script(&self, t: Timing) -> Result<Vec<TimedInput>, String> {
        let presses = self.presses(t)?;
        let frames = |n: u32| FrameRate::GBA.duration_of(u64::from(n));
        let mut timeline = Vec::new();
        if let Some(&(first, _, _)) = presses.first().filter(|p| p.0 > 0) {
            timeline.push(TimedInput {
                buttons: ButtonSet::NONE,
                duration: frames(first),
            });
        }
        for (i, &(at, pressed, hold)) in presses.iter().enumerate() {
            let next = presses.get(i + 1).map_or(at + hold + PRESS, |p| p.0);
            if next < at + hold + 1 {
                return Err(format!("presses at {at} and {next} overlap"));
            }
            timeline.push(TimedInput {
                buttons: pressed,
                duration: frames(hold),
            });
            timeline.push(TimedInput {
                buttons: ButtonSet::NONE,
                duration: frames(next - at - hold),
            });
        }
        if timeline.len() > MAX_STEPS {
            return Err(format!(
                "{} timeline steps; the ESP32 queues {MAX_STEPS}",
                timeline.len()
            ));
        }
        Ok(timeline)
    }
}

/// What an attempt ended with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptResult {
    pub timing: Timing,
    pub reading: Option<Reading>,
    pub shiny: Option<ShinyReading>,
    /// The scout's timings, when this attempt was the scout.
    pub scouted: Option<Scouted>,
    /// A press didn't land where the script needed it; the reading (if any)
    /// isn't from `timing`.
    pub desynced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Hard reset: waiting for the intro after the power cycle.
    Intro,
    // Scout (closed-loop) phases.
    Reset,
    ToTitle,
    Title,
    Menu,
    Recap,
    Talk,
    Dialogue,
    Last,
    // Scripted.
    Script,
    // Both: from the last press to the summary.
    Nickname,
    ToControl,
    OpenMenu,
    Party,
    Summary,
    Leave,
    Finished,
}

/// One attempt ([`StarterAttemptTask::scout`] or [`StarterAttemptTask::scripted`]).
pub struct StarterAttemptTask {
    phase: Phase,
    /// Scouted offsets (scripted attempts).
    scouted: Option<Scouted>,
    timing: Timing,
    /// Count time by frames (the stepped emulator) or by the wall clock.
    frame_clock: bool,
    started: Instant,
    // Scout marks (clock frames).
    t_reset: u64,
    t_title: u64,
    t_continue: u64,
    /// The last A the scout pressed in the ball's dialogue, and the page
    /// delay it confirms.
    t_prev: u64,
    pending_delay: u32,
    t_yes: u64,
    measured: Scouted,
    rival_received: bool,
    mash_select: bool,
    control_since: Option<u64>,
    nature: Option<u8>,
    shiny: Option<ShinyReading>,
    stats: Option<[u16; 6]>,
    candidate: Option<(u64, pokebot_state::SummaryObservation)>,
    /// Return to the field after reading (instead of stopping on the summary).
    leave: bool,
    quiet: u32,
    desynced: bool,
    timeouts: u32,
}

impl StarterAttemptTask {
    fn new(frame_clock: bool, reset: ResetKind, scouted: Option<Scouted>, timing: Timing) -> Self {
        Self {
            phase: match (reset, &scouted) {
                (ResetKind::Hard, _) => Phase::Intro,
                (ResetKind::Soft, Some(_)) => Phase::Script,
                (ResetKind::Soft, None) => Phase::Reset,
            },
            scouted,
            timing,
            frame_clock,
            started: Instant::now(),
            t_reset: 0,
            t_title: 0,
            t_continue: 0,
            t_prev: 0,
            pending_delay: 0,
            t_yes: 0,
            measured: Scouted {
                reset,
                ..Scouted::default()
            },
            rival_received: false,
            mash_select: true,
            control_since: None,
            nature: None,
            shiny: None,
            stats: None,
            candidate: None,
            leave: false,
            quiet: 0,
            desynced: false,
            timeouts: 0,
        }
    }

    /// Measures the screens' timings closed-loop, then reads its starter.
    /// With [`ResetKind::Hard`] the caller has just power cycled.
    pub fn scout(frame_clock: bool, reset: ResetKind) -> Self {
        Self::new(frame_clock, reset, None, Timing { title: 0, last: 0 })
    }

    /// Plays `timing` as one timed script from `scouted`.
    /// With [`ResetKind::Hard`] (from `scouted`) the caller has just power
    /// cycled.
    pub fn scripted(frame_clock: bool, scouted: Scouted, timing: Timing) -> Result<Self, String> {
        scouted.script(timing)?;
        Ok(Self::new(frame_clock, scouted.reset, Some(scouted), timing))
    }

    /// After reading, go back to the field and stand still for
    /// [`FIELD_IDLE`] frames: the next soft reset then comes from the same
    /// screen every time (and a shiny can be saved from there).
    pub fn leave_to_field(mut self) -> Self {
        self.leave = true;
        self
    }

    /// The attempt's outcome, once the task is done.
    pub fn result(&self) -> Option<AttemptResult> {
        if self.phase != Phase::Finished {
            return None;
        }
        let reading = match (self.nature, self.stats) {
            (Some(nature), Some(stats)) => Some(Reading { nature, stats }),
            _ => None,
        };
        Some(AttemptResult {
            timing: self.timing,
            reading,
            shiny: self.shiny,
            scouted: self.scouted.is_none().then(|| self.measured.clone()),
            desynced: self.desynced,
        })
    }

    /// Now, in frames: the observation's frame on the stepped emulator, the
    /// wall clock at the GBA's rate on real hardware.
    fn now(&self, o: &Observation) -> u64 {
        if self.frame_clock {
            o.frame_id
        } else {
            let rate = FrameRate::GBA;
            (self.started.elapsed().as_nanos() * u128::from(rate.numerator)
                / (1_000_000_000u128 * u128::from(rate.denominator))) as u64
        }
    }

    fn tap(label: &str, button: Button, expect: Expectation, timeout: u64) -> Decision {
        Decision::Act(Action::new(
            label,
            vec![ControllerCommand::Press(button)],
            expect,
            timeout,
        ))
    }

    fn desync(&mut self, why: &str) -> Decision {
        self.desynced = true;
        self.phase = Phase::Finished;
        Decision::Done(format!("attempt desynced: {why}"))
    }

    fn since(&self, o: &Observation, mark: u64) -> u32 {
        self.now(o).saturating_sub(mark) as u32
    }

    fn on_field(o: &Observation) -> bool {
        o.player.is_some() && o.dialogue.is_none() && o.menu.is_none() && o.party_menu.is_none()
    }

    fn scout_step(&mut self, o: &Observation) -> Decision {
        match self.phase {
            Phase::Reset => {
                self.t_reset = self.now(o);
                Decision::Act(Action::new(
                    "soft reset (A+B+Start+Select)",
                    vec![ControllerCommand::Hold {
                        buttons: buttons(&[Button::A, Button::B, Button::Start, Button::Select]),
                        duration: FrameRate::GBA.duration_of(u64::from(RESET_HOLD)),
                    }],
                    Expectation::InputsDone,
                    0,
                ))
            }
            Phase::ToTitle => {
                if o.screen.value == ScreenState::TitleScreen {
                    self.measured.title_seen = self.since(o, self.t_reset);
                    self.phase = Phase::Title;
                    return Decision::Wait("title screen: waiting for its RUN state".into());
                }
                let button = if self.mash_select {
                    Button::Select
                } else {
                    Button::B
                };
                self.mash_select = !self.mash_select;
                Self::tap("skip the intro", button, Expectation::InputsDone, 0)
            }
            Phase::Title => {
                if self.since(o, self.t_reset) < self.measured.title_min() {
                    return Decision::Wait("title screen: waiting for its RUN state".into());
                }
                self.t_title = self.now(o);
                self.timing.title = (self.t_title - self.t_reset) as u32;
                Self::tap(
                    "press A on the title screen",
                    Button::A,
                    Expectation::ScreenIsNot(ScreenState::TitleScreen),
                    300,
                )
            }
            Phase::Menu => match (&o.menu, o.screen.value) {
                (Some(menu), ScreenState::MainMenu) => {
                    if menu.cursor_row == 0 {
                        self.measured.menu_after_title = self.since(o, self.t_title);
                        self.t_continue = self.now(o);
                    }
                    select(menu, 0, "choose CONTINUE")
                }
                _ => Decision::Wait("waiting for the main menu".into()),
            },
            Phase::Recap => {
                if Self::on_field(o) {
                    let since = *self.control_since.get_or_insert(self.now(o));
                    if self.now(o) - since >= u64::from(CONTROL_FRAMES) {
                        self.measured.control_after_continue =
                            since.saturating_sub(self.t_continue) as u32;
                        self.phase = Phase::Talk;
                        return self.scout_step(o);
                    }
                    return Decision::Wait("confirming control on the field".into());
                }
                self.control_since = None;
                if o.screen.value == ScreenState::MainMenu {
                    return Decision::Wait("the main menu is closing".into());
                }
                Self::tap(
                    "press B to skip the recap",
                    Button::B,
                    Expectation::InputsDone,
                    0,
                )
            }
            Phase::Talk => {
                self.t_prev = self.now(o);
                Self::tap(
                    "talk to the Poké Ball",
                    Button::A,
                    Expectation::DialogueOpen,
                    300,
                )
            }
            Phase::Dialogue => match (&o.menu, &o.dialogue) {
                (Some(menu), Some(_)) => {
                    if menu.cursor_row == 0 {
                        self.pending_delay = self.since(o, self.t_prev);
                        self.t_yes = self.now(o);
                    }
                    select(menu, 0, "answer YES")
                }
                (None, Some(d)) if d.ready_for_a() => {
                    self.pending_delay = self.since(o, self.t_prev);
                    self.t_prev = self.now(o);
                    Decision::Act(Action::new(
                        "next page",
                        vec![ControllerCommand::Press(Button::A)],
                        Expectation::TextAdvanced {
                            kind: d.kind,
                            baseline: d.text_cells.clone(),
                        },
                        90,
                    ))
                }
                _ => Decision::Wait("waiting for the next page or the question".into()),
            },
            // The question's page lingers a moment after YES: wait for the
            // new text itself.
            Phase::Last => match &o.dialogue {
                Some(d) if d.ready_for_a() && o.menu.is_none() && says(o, "energetic") => {
                    self.measured.ready_after_yes = self.since(o, self.t_yes);
                    let now = self.now(o);
                    self.timing.last = (now - self.t_title) as u32;
                    Self::tap(
                        "close \"…really quite energetic!\" (the starter is made)",
                        Button::A,
                        Expectation::InputsDone,
                        0,
                    )
                }
                _ => Decision::Wait("waiting for \"…really quite energetic!\"".into()),
            },
            _ => unreachable!("not a scout phase"),
        }
    }

    fn read_summary(&mut self, o: &Observation) -> Decision {
        let Some(s) = &o.summary else {
            return Decision::Wait("waiting for the summary".into());
        };
        let stable = self
            .candidate
            .as_ref()
            .is_some_and(|(f, prev)| o.frame_id.saturating_sub(*f) >= SUMMARY_STABLE && prev == s);
        if !stable {
            if self.candidate.as_ref().is_none_or(|(_, prev)| prev != s) {
                self.candidate = Some((o.frame_id, s.clone()));
            }
            return Decision::Wait("confirming the summary on a later frame".into());
        }
        match s.page {
            SummaryPage::Info => {
                self.nature = s.details.nature.as_deref().and_then(nature_named);
                self.shiny = s.shiny;
                self.candidate = None;
                Self::tap(
                    "summary: SKILLS",
                    Button::Right,
                    Expectation::SummaryPage(SummaryPage::Skills),
                    90,
                )
            }
            SummaryPage::Skills => {
                let d = &s.details;
                if let (Some((_, hp)), Some(a), Some(df), Some(sp), Some(sa), Some(sd)) = (
                    s.hp,
                    d.attack,
                    d.defense,
                    d.speed,
                    d.sp_attack,
                    d.sp_defense,
                ) {
                    self.stats = Some([hp, a, df, sp, sa, sd]);
                }
                // The star shows on SKILLS too: a second look.
                if s.shiny == Some(ShinyReading::Shiny) && self.shiny != Some(ShinyReading::Shiny) {
                    self.shiny = Some(ShinyReading::Unclear);
                }
                if self.leave {
                    self.phase = Phase::Leave;
                    return Decision::Wait("leaving the summary".into());
                }
                self.phase = Phase::Finished;
                Decision::Done(format!(
                    "{} nature, stats {:?}, shiny {:?}",
                    self.nature
                        .map_or("?", |n| pokebot_gamedata::training::NATURES[usize::from(n)]),
                    self.stats,
                    self.shiny
                ))
            }
            SummaryPage::Moves => Self::tap(
                "summary: back to INFO",
                Button::Left,
                Expectation::SummaryPage(SummaryPage::Skills),
                90,
            ),
        }
    }
}

fn says(o: &Observation, word: &str) -> bool {
    o.dialogue
        .as_ref()
        .is_some_and(|d| d.lines.iter().any(|l| l.contains(word)))
}

impl Task for StarterAttemptTask {
    fn name(&self) -> &str {
        "ShinyStarterAttempt"
    }

    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.timeouts > 12 {
            return Decision::Fail(format!("stuck in {:?}", self.phase));
        }
        match self.phase {
            Phase::Intro => {
                if o.screen.value != ScreenState::Intro || o.screen.detector != "intro-game-freak" {
                    return Decision::Wait("waiting for the Game Freak intro".into());
                }
                self.t_reset = self.now(o);
                self.phase = if self.scouted.is_some() {
                    Phase::Script
                } else {
                    Phase::ToTitle
                };
                self.next(ctx)
            }
            Phase::Reset
            | Phase::ToTitle
            | Phase::Title
            | Phase::Menu
            | Phase::Recap
            | Phase::Talk
            | Phase::Dialogue
            | Phase::Last => self.scout_step(o),
            Phase::Script => {
                let scouted = self.scouted.as_ref().expect("scripted");
                let script = match scouted.script(self.timing) {
                    Ok(s) => s,
                    Err(e) => return Decision::Fail(e),
                };
                let frames: u64 = script
                    .iter()
                    .map(|i| FrameRate::GBA.frames_for(i.duration))
                    .sum();
                Decision::Act(Action::new(
                    format!(
                        "timed attempt: title press at {} frames, last A {} frames later",
                        self.timing.title, self.timing.last
                    ),
                    vec![ControllerCommand::Sequence(script)],
                    Expectation::InputsDone,
                    frames + 300,
                ))
            }
            Phase::Nickname => {
                // Checked first: an A here would make the starter at an
                // unknown frame.
                if o.menu.is_none() && says(o, "energetic") {
                    return self.desync("\"…energetic!\" still waits for the last A");
                }
                if let (Some(menu), Some(_)) = (&o.menu, &o.dialogue) {
                    if says(o, "nickname") {
                        return select(menu, 1, "no nickname");
                    }
                    if says(o, "want") || says(o, "POKéMON") {
                        return self.desync("the starter question is still up");
                    }
                    return Decision::Wait("reading the question".into());
                }
                if Self::on_field(o) {
                    return self.desync("back on the field without the nickname question");
                }
                Decision::Wait("waiting for the nickname question".into())
            }
            Phase::ToControl => {
                // The rival walks to the table without a text box: control
                // only after "GREEN received the …!".
                if says(o, "received") {
                    self.rival_received = true;
                }
                if self.rival_received && Self::on_field(o) {
                    let now = self.now(o);
                    let since = *self.control_since.get_or_insert(now);
                    if now - since >= 30 {
                        self.phase = Phase::OpenMenu;
                        return self.next(ctx);
                    }
                    return Decision::Wait("confirming control on the field".into());
                }
                self.control_since = None;
                advance_or_wait(o.dialogue.as_ref(), "the rival takes his Poké Ball")
            }
            Phase::OpenMenu => match &o.menu {
                Some(menu) if o.party_menu.is_none() => select(menu, 0, "open POKéMON"),
                None if o.party_menu.is_some() => {
                    self.phase = Phase::Party;
                    self.next(ctx)
                }
                None if Self::on_field(o) => Self::tap(
                    "open the Start menu",
                    Button::Start,
                    Expectation::MenuOpen,
                    60,
                ),
                _ => Decision::Wait("waiting for the party menu".into()),
            },
            Phase::Party => {
                if o.summary.is_some() {
                    self.phase = Phase::Summary;
                    return self.next(ctx);
                }
                match &o.party_menu {
                    Some(m) if m.actions => Self::tap(
                        "SUMMARY",
                        Button::A,
                        Expectation::SummaryPage(SummaryPage::Info),
                        120,
                    ),
                    Some(m) if m.selected == Some(0) => Self::tap(
                        "choose the starter",
                        Button::A,
                        Expectation::PartyActions,
                        90,
                    ),
                    _ => Decision::Wait("waiting for the party list".into()),
                }
            }
            Phase::Summary => self.read_summary(o),
            Phase::Leave => {
                if Self::on_field(o) {
                    self.quiet += 1;
                    if self.quiet >= FIELD_IDLE {
                        self.phase = Phase::Finished;
                        return Decision::Done("read; back on the field".into());
                    }
                    return Decision::Wait("standing still on the field".into());
                }
                self.quiet = 0;
                Self::tap("back to the field", Button::B, Expectation::InputsDone, 0)
            }
            Phase::Finished => Decision::Done("attempt finished".into()),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _ctx: &mut TaskContext<'_>) {
        if outcome == Outcome::TimedOut {
            self.timeouts += 1;
            if self.phase == Phase::Title {
                // The A missed the title's RUN state: measure again.
                self.phase = Phase::Reset;
            }
            return;
        }
        let label = action.label.as_str();
        self.phase = match (self.phase, label) {
            (Phase::Reset, _) => Phase::ToTitle,
            (Phase::Title, _) => Phase::Menu,
            (Phase::Menu, "choose CONTINUE") => Phase::Recap,
            (Phase::Talk, _) => Phase::Dialogue,
            (Phase::Dialogue, "next page" | "answer YES") => {
                self.measured.dialogue.push(self.pending_delay);
                if label == "answer YES" {
                    Phase::Last
                } else {
                    Phase::Dialogue
                }
            }
            (Phase::Last, _) | (Phase::Script, _) => Phase::Nickname,
            (Phase::Nickname, "no nickname") => {
                self.control_since = None;
                Phase::ToControl
            }
            (phase, _) => phase,
        };
    }
}

/// What a hunt keeps between runs: the scouted timings and every attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HuntFile {
    pub starter: crate::Starter,
    pub scouted: Option<Scouted>,
    pub hunt: Option<crate::shiny_plan::Hunt>,
    /// Where the save stands, facing the ball (from `--prepare`).
    #[serde(default)]
    pub stand: Option<pokebot_state::PlayerPose>,
}

impl HuntFile {
    /// The hunt at `path`, or a fresh one when there's no file yet.
    pub fn load(path: &std::path::Path, starter: crate::Starter) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let file: Self = serde_json::from_str(&text)
                    .map_err(|e| format!("parsing {}: {e}", path.display()))?;
                if file.starter != starter {
                    return Err(format!(
                        "{} hunts {:?}, not {starter:?}",
                        path.display(),
                        file.starter
                    ));
                }
                Ok(file)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                starter,
                scouted: None,
                hunt: None,
                stand: None,
            }),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    pub fn store(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

/// How long a scripted attempt's timeline plays.
pub fn script_duration(scouted: &Scouted, timing: Timing) -> Option<Duration> {
    Some(
        scouted
            .script(timing)
            .ok()?
            .iter()
            .map(|i| i.duration)
            .sum(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scouted() -> Scouted {
        Scouted {
            reset: ResetKind::Soft,
            title_seen: 300,
            menu_after_title: 180,
            control_after_continue: 250,
            dialogue: vec![150, 90],
            ready_after_yes: 70,
        }
    }

    #[test]
    fn the_script_presses_at_the_planned_frames() {
        let s = scouted();
        let t = Timing {
            title: s.title_min() + 7,
            last: s.last_min() + 11,
        };
        let script = s.script(t).unwrap();
        // Replay the timeline frame by frame; note each A press's first frame.
        let mut frame = 0u64;
        let mut a_presses = Vec::new();
        let mut held = false;
        for input in &script {
            let n = FrameRate::GBA.frames_for(input.duration);
            let a = input.buttons.contains(Button::A) && !input.buttons.contains(Button::B);
            if a && !held {
                a_presses.push(frame);
            }
            held = a;
            frame += n;
        }
        assert_eq!(a_presses.first(), Some(&u64::from(t.title)));
        assert_eq!(a_presses.last(), Some(&u64::from(t.title + t.last)));
        assert_eq!(a_presses.len(), 6, "title, CONTINUE, talk, page, YES, last");
        assert!(script.len() <= MAX_STEPS);
    }

    #[test]
    fn a_hard_reset_script_starts_with_the_intro_skip() {
        let s = Scouted {
            reset: ResetKind::Hard,
            title_seen: 24,
            ..scouted()
        };
        let t = Timing {
            title: s.title_min(),
            last: s.last_min(),
        };
        let script = s.script(t).unwrap();
        assert_eq!(script[0].buttons, ButtonSet::NONE);
        assert_eq!(
            FrameRate::GBA.frames_for(script[0].duration),
            u64::from(INTRO_SKIP)
        );
        assert!(script[1].buttons.contains(Button::Select));
        let title_at: u64 = script
            .iter()
            .take_while(|i| !i.buttons.contains(Button::A))
            .map(|i| FrameRate::GBA.frames_for(i.duration))
            .sum();
        assert_eq!(title_at, u64::from(t.title));
    }

    #[test]
    fn b_and_select_are_never_held_together_after_the_reset() {
        let s = scouted();
        let script = s
            .script(Timing {
                title: s.title_min(),
                last: s.last_min(),
            })
            .unwrap();
        for input in &script[2..] {
            assert!(!(input.buttons.contains(Button::B) && input.buttons.contains(Button::Select)));
        }
    }

    #[test]
    fn too_early_timings_are_refused() {
        let s = scouted();
        assert!(s
            .script(Timing {
                title: s.title_min() - 1,
                last: s.last_min(),
            })
            .is_err());
        assert!(s
            .script(Timing {
                title: s.title_min(),
                last: s.last_min() - 1,
            })
            .is_err());
    }
}
