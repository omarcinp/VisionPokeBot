//! The bot loop shared by every front end:
//!
//! ```text
//! VideoSource → Normalizer → Perception → EventExtractor → Reducer → GameState
//!                                                   ▲
//! Controller  ◀── commands ─────────────────────────┘ (InputIssued events)
//! ```
//!
//! Recording and telemetry are optional side outputs; neither can influence
//! what the bot decides.

use std::path::Path;
use std::time::Instant;

use pokebot_core::{
    CapturedFrame, ConsoleLink, Controller, ControllerCommand, ControllerReceipt, NormalizedFrame,
    Result, VideoSource,
};
use pokebot_replay::SessionRecorder;
use pokebot_state::{
    DefaultReducer, EventExtractor, EventRecord, FrameArrival, GameEvent, GameState, Observation,
    StateReducer,
};
use pokebot_telemetry::{LogKind, Telemetry};
use pokebot_video::Normalizer;
use pokebot_vision::{FireRedPerception, PerceptionSystem};

pub struct Devices {
    pub video: Box<dyn VideoSource + Send>,
    pub controller: Box<dyn Controller + Send>,
    pub normalizer: Normalizer,
    pub video_name: String,
    pub controller_name: String,
    /// Persists the cartridge save right away (emulator only; a real
    /// cartridge writes its own).
    pub persist_save: Option<Box<dyn Fn() -> Result<()> + Send>>,
}

pub struct Runtime {
    pub devices: Devices,
    perception: Box<dyn PerceptionSystem + Send>,
    extractor: EventExtractor,
    reducer: DefaultReducer,
    state: GameState,
    observation: Option<Observation>,
    last_frame: Option<NormalizedFrame>,
    /// The last frame as captured (full resolution), for debug bundles.
    last_captured: Option<CapturedFrame>,
    /// The console shows something other than the game (see
    /// `Normalizer::outside_viewport_lit`).
    outside_game: bool,
    /// The whole captured frame is black (see `Normalizer::dark`).
    dark: bool,
    frames_seen: u64,
    recorder: Option<SessionRecorder>,
    /// Per-stage timing of `observe` (`VPB_PROFILE=1`), reported every
    /// [`Profile::EVERY`] frames on stderr.
    profile: Option<Profile>,
    telemetry: Option<Telemetry>,
    echo_events: bool,
    control: Option<pokebot_telemetry::GameControl>,
}

impl Runtime {
    pub fn new(devices: Devices) -> Self {
        Self::with_perception(devices, FireRedPerception::default())
    }

    pub fn with_perception(devices: Devices, perception: FireRedPerception) -> Self {
        Self::with_perception_system(devices, Box::new(perception))
    }

    /// Any perception (tests script observations through one).
    pub fn with_perception_system(
        devices: Devices,
        perception: Box<dyn PerceptionSystem + Send>,
    ) -> Self {
        Self {
            devices,
            perception,
            extractor: EventExtractor::default(),
            reducer: DefaultReducer,
            state: GameState::default(),
            observation: None,
            last_frame: None,
            last_captured: None,
            outside_game: false,
            dark: false,
            frames_seen: 0,
            recorder: None,
            profile: std::env::var_os("VPB_PROFILE").map(|_| Profile::default()),
            telemetry: None,
            echo_events: false,
            control: None,
        }
    }

    pub fn record_to(&mut self, dir: &Path, record_raw: bool, stride: u64) -> Result<()> {
        let recorder = SessionRecorder::create(
            dir,
            &self.devices.video_name,
            &self.devices.controller_name,
            record_raw,
        )?
        .with_stride(stride);
        self.info(format!(
            "recording session to {}{}",
            dir.display(),
            if stride > 1 {
                format!(" (every {stride}th frame)")
            } else {
                String::new()
            }
        ));
        self.recorder = Some(recorder);
        Ok(())
    }

    /// Also print semantic events to stderr.
    pub fn echo_events(&mut self, echo: bool) {
        self.echo_events = echo;
    }

    pub fn attach_telemetry(&mut self, telemetry: Telemetry) {
        telemetry.info(format!(
            "devices: video={} controller={}",
            self.devices.video_name, self.devices.controller_name
        ));
        self.telemetry = Some(telemetry);
    }

    pub fn enable_web_control(&mut self) -> pokebot_telemetry::GameControl {
        let controller = std::mem::replace(
            &mut self.devices.controller,
            Box::new(pokebot_controller::NullController::new(
                pokebot_core::PressProfile::default(),
            )),
        );
        let control = pokebot_telemetry::GameControl::new(controller, true);
        self.devices.controller = control.bot_controller();
        self.control = Some(control.clone());
        control
    }

    pub fn stop_web_bot(&self) -> Result<()> {
        if let Some(control) = &self.control {
            control
                .request(serde_json::json!({"action":"stop"}))
                .map_err(pokebot_core::Error::Device)?;
        }
        Ok(())
    }

    pub fn bot_stop_signal(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.control.as_ref().map(|c| c.stop_signal())
    }

    pub fn bot_stopped(&self) -> bool {
        self.control.as_ref().is_some_and(|c| !c.bot_enabled())
    }

    pub fn allow_web_resume(&self, allow: bool) {
        if let Some(control) = &self.control {
            control.allow_resume(allow);
        }
    }

    /// Reads, normalizes and interprets the next frame.
    pub fn observe(&mut self) -> Result<&NormalizedFrame> {
        // Human input uses wall-clock durations. Keep a stepped emulator near
        // console speed while stopped or under manual control.
        if self.bot_stopped() && self.devices.video_name.starts_with("emulator:") {
            std::thread::sleep(std::time::Duration::from_micros(16_743));
        }
        let mut lap = self.profile.as_mut().map(|p| p.lap());
        let captured = self.devices.video.next_frame()?;
        if let Some(l) = lap.as_mut() {
            l.mark(0);
        }
        let frame = self.devices.normalizer.normalize(&captured)?;
        self.outside_game = self.devices.normalizer.outside_viewport_lit(&captured);
        self.dark = Normalizer::dark(&captured);
        if let Some(l) = lap.as_mut() {
            l.mark(1);
        }
        let observation = self.perception.observe(&frame);
        if let Some(l) = lap.as_mut() {
            l.mark(2);
        }
        let events = self
            .extractor
            .observe(&observation, FrameArrival::from(&captured));
        self.apply(&events)?;
        if let Some(l) = lap.as_mut() {
            l.mark(3);
        }
        if let Some(recorder) = &mut self.recorder {
            recorder.record_frame(&captured, &frame)?;
        }
        if let Some(l) = lap.as_mut() {
            l.mark(4);
        }
        if let Some(telemetry) = &self.telemetry {
            telemetry.publish_frame(&frame, &self.state, &observation);
        }
        if let Some(l) = lap.as_mut() {
            l.mark(5);
        }
        if let (Some(lap), Some(profile)) = (lap, self.profile.as_mut()) {
            if let Some(report) = profile.add(lap) {
                eprintln!("{report}");
            }
        }
        self.frames_seen += 1;
        self.observation = Some(observation);
        self.last_captured = Some(captured);
        Ok(self.last_frame.insert(frame))
    }

    pub fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        let receipt = match self.devices.controller.execute(command.clone()) {
            Ok(receipt) => receipt,
            Err(e) => {
                self.error(format!("controller rejected {command:?}: {e}"));
                return Err(e);
            }
        };
        if let Some(recorder) = &mut self.recorder {
            recorder.record_command(&command, &receipt)?;
        }
        let event = self.extractor.input(receipt.command_id, &command);
        if let Some(telemetry) = &self.telemetry {
            telemetry.log(
                LogKind::Action,
                Some(event.frame_id),
                format!(
                    "#{} {} ({} ms)",
                    receipt.command_id,
                    describe(&command),
                    receipt.input_duration.as_millis()
                ),
                &command,
            );
        }
        self.apply(std::slice::from_ref(&event))?;
        Ok(receipt)
    }

    /// Records an event produced outside perception (by the agent), e.g. a
    /// goal changing phase or a choice confirmed on screen.
    pub fn emit(&mut self, event: GameEvent) -> Result<()> {
        let frame_id = self.last_frame.as_ref().map_or(0, |f| f.frame_id);
        self.apply(&[EventRecord { frame_id, event }])
    }

    /// Logs a planner decision (shown as "goal" entries in the web UI).
    pub fn explain(&self, summary: impl Into<String>, detail: &impl serde::Serialize) {
        if let Some(telemetry) = &self.telemetry {
            let frame_id = self.last_frame.as_ref().map(|f| f.frame_id);
            telemetry.log(LogKind::Goal, frame_id, summary, detail);
        }
    }

    /// Appends a diagnostic record (not a game event) to the recording, e.g.
    /// how long an action took to confirm.
    pub fn record(&mut self, kind: &str, detail: impl serde::Serialize) -> Result<()> {
        let frame_id = self.last_frame.as_ref().map_or(0, |f| f.frame_id);
        if let Some(recorder) = &mut self.recorder {
            recorder.record_event(&serde_json::json!({
                "frame_id": frame_id,
                "event": { kind: detail },
            }))?;
        }
        Ok(())
    }

    /// Makes an in-game save durable now (no-op on real hardware).
    pub fn persist_save(&self) -> Result<()> {
        self.devices
            .persist_save
            .as_ref()
            .map_or(Ok(()), |persist| persist())
    }

    /// Tells perception where the player is believed to be.
    pub fn set_pose_hint(&mut self, pose: pokebot_state::PlayerPose) {
        self.perception.set_pose_hint(pose);
    }

    /// Forgets where the player was believed to be: the next frames are
    /// located from scratch (the goal loop's `locate_anywhere`).
    pub fn clear_pose_hint(&mut self) {
        self.perception.clear_pose_hint();
    }

    /// Observation of the most recent frame.
    pub fn observation(&self) -> Option<&Observation> {
        self.observation.as_ref()
    }

    pub fn is_idle(&self) -> Result<bool> {
        self.devices.controller.is_idle()
    }

    pub fn state(&self) -> &GameState {
        &self.state
    }

    pub fn last_frame(&self) -> Option<&NormalizedFrame> {
        self.last_frame.as_ref()
    }

    /// The last frame as captured, before normalization.
    pub fn last_captured(&self) -> Option<&CapturedFrame> {
        self.last_captured.as_ref()
    }

    /// The console is showing something other than the game.
    pub fn outside_game(&self) -> bool {
        self.outside_game
    }

    /// The last captured frame was entirely black.
    pub fn dark(&self) -> bool {
        self.dark
    }

    /// The video is a console seen through a capture card (a fixed viewport
    /// in a bigger frame), not the game alone.
    pub fn watches_console(&self) -> bool {
        self.devices.normalizer.letterboxed()
    }

    /// What the controller knows about the console's USB connection.
    pub fn console_link(&mut self) -> Result<ConsoleLink> {
        self.devices.controller.console_link()
    }

    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    pub fn task_finished(&self, success: bool) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.task_finished(success);
        }
    }

    pub fn info(&self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("{message}");
        if let Some(telemetry) = &self.telemetry {
            telemetry.info(message);
        }
    }

    pub fn error(&self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("error: {message}");
        if let Some(telemetry) = &self.telemetry {
            telemetry.error(message);
        }
    }

    pub fn finish(mut self) -> Result<()> {
        // The web controller retains an emulator handle until process exit.
        // Flush explicitly instead of relying on the last handle's destructor.
        self.persist_save()?;
        if let Some(recorder) = &mut self.recorder {
            recorder.flush()?;
        }
        Ok(())
    }

    fn apply(&mut self, events: &[EventRecord]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.state = self.reducer.reduce(&self.state, events);
        for record in events {
            if let Some(recorder) = &mut self.recorder {
                recorder.record_event(record)?;
            }
            // Inputs are already logged as actions.
            if matches!(record.event, GameEvent::InputIssued { .. }) {
                continue;
            }
            let kind = match record.event {
                GameEvent::GoalProgress { .. } | GameEvent::GoalFinished { .. } => LogKind::Goal,
                _ => LogKind::Event,
            };
            if self.echo_events {
                eprintln!("[{}] {}", record.frame_id, summarize(&record.event));
            }
            if let Some(telemetry) = &self.telemetry {
                telemetry.log(
                    kind,
                    Some(record.frame_id),
                    summarize(&record.event),
                    &record.event,
                );
            }
        }
        Ok(())
    }
}

fn describe(command: &ControllerCommand) -> String {
    match command {
        ControllerCommand::Press(button) => format!("Press {button}"),
        ControllerCommand::Chord(buttons) => format!("Chord {buttons}"),
        ControllerCommand::Hold { buttons, duration } => {
            format!("Hold {buttons} {} ms", duration.as_millis())
        }
        ControllerCommand::Sequence(inputs) => format!("Sequence of {}", inputs.len()),
        ControllerCommand::Neutral => "Neutral".into(),
    }
}

fn summarize(event: &GameEvent) -> String {
    match event {
        GameEvent::GoalProgress {
            goal,
            phase,
            detail,
        } => format!("{goal}: {phase} — {detail}"),
        GameEvent::GoalFinished {
            goal,
            success,
            detail,
        } => {
            format!(
                "{goal} {} — {detail}",
                if *success { "completed" } else { "failed" }
            )
        }
        GameEvent::GenderChosen { gender } => format!("GenderChosen {gender:?}"),
        GameEvent::PlayerNamed { name } => format!("PlayerNamed {name}"),
        GameEvent::RivalNamed { name } => format!("RivalNamed {name}"),
        GameEvent::ControlConfirmed => "ControlConfirmed (Start menu opened and closed)".into(),
        GameEvent::GameSaved => "GameSaved (in-game save completed)".into(),
        GameEvent::BattleStarted => "BattleStarted".into(),
        GameEvent::BattleEnded => "BattleEnded".into(),
        GameEvent::PlayerLocated { pose } => format!("PlayerLocated {pose}"),
        GameEvent::PlayerMoved { from, to } => format!(
            "PlayerMoved ({}, {}) → ({}, {})",
            from.x, from.y, to.x, to.y
        ),
        GameEvent::MapChanged { from, to } => format!("MapChanged {} → {to}", from.map),
        GameEvent::ScreenChanged { from, to, detector } => {
            format!("ScreenChanged {from:?} → {to:?} ({detector})")
        }
        GameEvent::InputIssued {
            command_id,
            command,
        } => format!("InputIssued #{command_id} {}", describe(command)),
        GameEvent::FramesDroppedByCard {
            missing,
            delivered,
            window_ms,
        } => format!(
            "FramesDroppedByCard {missing} in {} ({}% of the frames, {} delivered)",
            seconds(*window_ms),
            percent(*missing, *delivered),
            per_second(*delivered, *window_ms)
        ),
        GameEvent::FramesDroppedByProcessing {
            missing,
            seen,
            window_ms,
        } => format!(
            "FramesDroppedByProcessing {missing} in {} ({}% of the delivered frames, {} processed)",
            seconds(*window_ms),
            percent(*missing, *seen),
            per_second(*seen, *window_ms)
        ),
        GameEvent::PartyMonDerived { slot, mon } => format!(
            "Party {slot}: {} Lv{} (derived)",
            mon.species.value.as_deref().unwrap_or("?"),
            mon.level.value.map_or("?".into(), |l| l.to_string())
        ),
        GameEvent::PartyObserved {
            slot,
            species,
            level,
            hp,
            ..
        } => {
            format!("Party {slot} seen: {species:?} Lv{level:?} HP {hp:?}")
        }
        GameEvent::PartyAudited { members } => format!(
            "Party audited: {} members, HP {:?}",
            members.len(),
            members.iter().map(|m| m.hp.value).collect::<Vec<_>>()
        ),
        GameEvent::MovesObserved { slot, moves } => format!("Party {slot} moves {moves:?}"),
        GameEvent::MovePpObserved {
            slot,
            move_slot,
            cur,
            max,
        } => {
            format!("Party {slot} move {move_slot} PP {cur}/{max}")
        }
        GameEvent::MoveUsed { slot, move_slot } => format!("Party {slot} used move {move_slot}"),
        GameEvent::MoveLearned { slot, mv, .. } => format!("Party {slot} learned {mv}"),
        GameEvent::MoveReplaced { slot, old, new, .. } => format!("Party {slot}: {old} → {new}"),
        GameEvent::MoveOutOfPp { slot, move_slot } => {
            format!("Party {slot} move {move_slot} has no PP")
        }
        GameEvent::Evolved { slot, species } => format!("Party {slot} evolved into {species}"),
        GameEvent::Healed => "Healed (party restored)".into(),
        GameEvent::ItemsChanged {
            item,
            delta,
            reason,
            ..
        } => format!("Bag {item} {delta:+} ({reason})"),
        GameEvent::PocketObserved { pocket, items } => {
            format!("Bag {pocket:?}: {} items seen", items.len())
        }
        GameEvent::MoneyObserved { amount } => format!("Money ₽{amount}"),
        GameEvent::MoneyChanged { delta, reason } => format!("Money {delta:+} ({reason})"),
        GameEvent::BoxObserved { box_index, mons } => {
            format!("PC box {}: {} Pokémon", box_index + 1, mons.len())
        }
        GameEvent::PcItemsObserved { items } => format!("PC items: {}", items.len()),
        GameEvent::SentToPc { box_index, mon } => format!(
            "{:?} sent to PC box {:?}",
            mon.species.value,
            box_index.map(|b| b + 1)
        ),
        GameEvent::MonDeposited {
            party_slot,
            box_index,
        } => format!("Party {party_slot} deposited in box {}", box_index + 1),
        GameEvent::MonWithdrawn {
            box_index,
            box_slot,
        } => format!("Withdrew box {} slot {box_slot}", box_index + 1),
        GameEvent::SpeciesSeen { species } => format!("Seen {species}"),
        GameEvent::SpeciesCaught { species } => format!("Caught {species}"),
        GameEvent::PokedexCountObserved { seen, caught } => match seen {
            Some(seen) => format!("Pokédex {seen} seen, {caught} caught"),
            None => format!("Pokédex {caught} caught"),
        },
        GameEvent::ShinySeen { species } => format!("SHINY {species}!"),
        GameEvent::BadgeEarned { badge } => format!("Badge earned: {badge}"),
        GameEvent::CheckpointRestored { .. } => "Checkpoint knowledge restored".into(),
        GameEvent::FlagObserved { flag, value } => format!("Flag {flag} = {value} (seen)"),
        GameEvent::FlagTracked { flag, value } => format!("Flag {flag} = {value} (tracked)"),
        GameEvent::VarObserved { var, value } => format!("Var {var} = {value} (seen)"),
        GameEvent::VarTracked { var, value } => format!("Var {var} = {value} (tracked)"),
        GameEvent::MapVisited { map } => format!("Visited {map}"),
        GameEvent::RespawnSet { map, x, y } => format!("Respawn at {map} ({x}, {y})"),
        GameEvent::NpcSeen {
            map,
            local_id,
            x,
            y,
            facing,
        } => format!("NPC {map}#{local_id} at ({x}, {y}) facing {facing:?}"),
        GameEvent::NpcAbsent { map, local_id } => format!("NPC {map}#{local_id} absent"),
        GameEvent::ScriptPathRun { script, path } => format!("Ran {script} path {path}"),
        GameEvent::IntentInfeasible { intent } => format!("Intent {intent} infeasible"),
        GameEvent::TileBlocked { map, x, y } => format!("Tile {map} ({x}, {y}) blocked (learnt)"),
        GameEvent::WhitedOut => "Whited out: the lead fainted (HP 0)".into(),
        GameEvent::TileUnblocked { map, x, y } => {
            format!("Tile {map} ({x}, {y}) not blocked after all (something came up)")
        }
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

/// `missing` as a share of `missing + kept`.
fn percent(missing: u64, kept: u64) -> u64 {
    missing * 100 / (missing + kept).max(1)
}

fn per_second(frames: u64, ms: u64) -> String {
    format!("{:.0} fps", frames as f64 * 1000.0 / ms.max(1) as f64)
}

#[cfg(test)]
mod tests {
    use pokebot_controller::NullController;
    use pokebot_core::{Button, CapturedFrame, Error, RgbImage};
    use pokebot_state::ScreenState;
    use pokebot_video::ViewportLocator;

    use super::*;

    /// Plays a fixed list of frames, like a tiny capture device.
    struct Frames(std::vec::IntoIter<RgbImage>, u64);

    impl VideoSource for Frames {
        fn next_frame(&mut self) -> Result<CapturedFrame> {
            let image = self.0.next().ok_or(Error::EndOfStream)?;
            self.1 += 1;
            Ok(CapturedFrame {
                frame_id: self.1 - 1,
                delivered: self.1 - 1,
                captured_at: std::time::Instant::now(),
                image,
            })
        }
    }

    #[test]
    fn shutdown_flushes_save_even_when_web_control_retains_the_device() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let saved = Arc::new(AtomicBool::new(false));
        let written = saved.clone();
        let mut runtime = Runtime::new(Devices {
            video: Box::new(Frames(Vec::new().into_iter(), 0)),
            controller: Box::new(NullController::default()),
            normalizer: Normalizer::new(ViewportLocator::FullFrame),
            video_name: "test".into(),
            controller_name: "null".into(),
            persist_save: Some(Box::new(move || {
                written.store(true, Ordering::Relaxed);
                Ok(())
            })),
        });
        let control = runtime.enable_web_control();
        runtime.finish().unwrap();
        assert!(saved.load(Ordering::Relaxed));
        assert!(control.bot_enabled());
    }

    #[test]
    fn frames_and_inputs_flow_into_state() {
        let busy = {
            let mut img = RgbImage::filled(240, 160, [0, 0, 0]);
            for x in 0..240 {
                for y in 0..40 {
                    img.put_pixel(x, y, [220, 120, 40]);
                }
            }
            img
        };
        let black = RgbImage::filled(240, 160, [0, 0, 0]);
        let frames = vec![busy.clone(), black.clone(), black, busy.clone(), busy];
        let mut runtime = Runtime::new(Devices {
            video: Box::new(Frames(frames.into_iter(), 0)),
            controller: Box::new(NullController::default()),
            normalizer: Normalizer::new(ViewportLocator::FullFrame),
            video_name: "test".into(),
            controller_name: "null".into(),
            persist_save: None,
        });
        let telemetry = Telemetry::new("test", "null");
        runtime.attach_telemetry(telemetry);

        runtime.observe().unwrap();
        runtime
            .execute(ControllerCommand::Press(Button::A))
            .unwrap();
        runtime.observe().unwrap();
        runtime.observe().unwrap();
        assert_eq!(runtime.state().screen.value, Some(ScreenState::Transition));
        assert_eq!(runtime.state().input.commands_issued, 1);
        assert_eq!(runtime.state().input.last_command_frame, Some(0));
        runtime.observe().unwrap();
        runtime.observe().unwrap();
        assert_eq!(runtime.state().screen.value, None, "back to not knowing");
        assert!(matches!(runtime.observe(), Err(Error::EndOfStream)));
    }
}

/// Where the frame loop's time goes: capture (the device, or the stepped
/// emulator's frame), normalize, perceive, extract + reduce, record,
/// telemetry. Between `observe` calls the caller (executor, tools, planner)
/// runs; that gap is reported as `agent`.
#[derive(Debug, Default)]
struct Profile {
    sums: [f64; 7],
    frames: u64,
    last_end: Option<Instant>,
}

struct Lap {
    start: Instant,
    marks: [f64; 6],
    gap: f64,
}

impl Lap {
    fn mark(&mut self, i: usize) {
        self.marks[i] = self.start.elapsed().as_secs_f64() * 1000.0;
    }
}

impl Profile {
    const EVERY: u64 = 600;
    const NAMES: [&'static str; 7] = [
        "capture",
        "normalize",
        "perceive",
        "extract",
        "record",
        "telemetry",
        "agent",
    ];

    fn lap(&mut self) -> Lap {
        let now = Instant::now();
        let gap = self
            .last_end
            .map_or(0.0, |e| now.duration_since(e).as_secs_f64() * 1000.0);
        Lap {
            start: now,
            marks: [0.0; 6],
            gap,
        }
    }

    fn add(&mut self, lap: Lap) -> Option<String> {
        let mut prev = 0.0;
        for (i, m) in lap.marks.iter().enumerate() {
            self.sums[i] += m - prev;
            prev = *m;
        }
        self.sums[6] += lap.gap;
        self.frames += 1;
        self.last_end = Some(Instant::now());
        if self.frames % Self::EVERY != 0 {
            return None;
        }
        let n = self.frames as f64;
        let total: f64 = self.sums.iter().sum::<f64>() / n;
        let parts: Vec<String> = Self::NAMES
            .iter()
            .zip(self.sums.iter())
            .map(|(name, sum)| format!("{name} {:.2}", sum / n))
            .collect();
        Some(format!(
            "profile: {total:.2} ms/frame over {} frames: {}",
            self.frames,
            parts.join(", ")
        ))
    }
}
