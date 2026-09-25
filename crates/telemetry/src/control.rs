//! Exclusive controller ownership for a bot and one browser. Manual inputs
//! are short leases: a lost tab cannot leave a direction held indefinitely.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_core::{
    ButtonSet, ConsoleLink, Controller, ControllerCommand, ControllerReceipt, Error, Result,
};
use serde_json::{json, Value};

const LEASE: Duration = Duration::from_secs(2);
const HOLD: Duration = Duration::from_millis(180);

#[derive(Clone)]
pub struct GameControl(Arc<Mutex<State>>, Arc<AtomicBool>);

struct State {
    controller: Box<dyn Controller + Send>,
    bot: bool,
    resume: bool,
    owner: Option<String>,
    renewed: Instant,
    sequence: u64,
    shutdown: Option<Arc<AtomicBool>>,
    state_file: Option<std::path::PathBuf>,
}

impl GameControl {
    pub fn new(controller: Box<dyn Controller + Send>, bot: bool) -> Self {
        Self(
            Arc::new(Mutex::new(State {
                controller,
                bot,
                resume: false,
                owner: None,
                renewed: Instant::now(),
                sequence: 0,
                shutdown: None,
                state_file: None,
            })),
            Arc::new(AtomicBool::new(!bot)),
        )
    }

    pub fn bot_controller(&self) -> Box<dyn Controller + Send> {
        Box::new(BotController(self.clone()))
    }

    pub fn persist_stopped_state(&self, path: std::path::PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if path.exists() {
            s.bot = false;
            self.1.store(true, Ordering::Relaxed);
        }
        s.state_file = Some(path);
        Ok(())
    }

    pub fn set_shutdown(&self, stop: Arc<AtomicBool>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).shutdown = Some(stop);
    }

    pub fn stop_signal(&self) -> Arc<AtomicBool> {
        self.1.clone()
    }

    pub fn bot_enabled(&self) -> bool {
        !self.1.load(Ordering::Relaxed)
    }

    /// Only advertise resume after the old task has unwound. The goal runner
    /// starts a fresh cycle from the current screen, never a suspended action.
    pub fn allow_resume(&self, allow: bool) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).resume = allow;
    }

    pub fn status(&self) -> Value {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.expire();
        json!({"mode": if s.bot { "bot" } else if s.owner.is_some() { "manual" } else { "stopped" },
            "can_resume": s.resume, "can_shutdown": s.shutdown.is_some(), "lease_ms": LEASE.as_millis(), "bot_enabled": s.bot})
    }

    pub fn request(&self, body: Value) -> std::result::Result<Value, String> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.expire();
        let action = body["action"].as_str().ok_or("action is required")?;
        let token = body["owner"].as_str().unwrap_or_default();
        let valid_token = !token.is_empty() && token.len() <= 100;
        let neutral = |s: &mut State| {
            s.controller
                .execute(ControllerCommand::Neutral)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        match action {
            "take" => {
                if !valid_token {
                    return Err("owner token is required".into());
                }
                if s.owner.as_deref().is_some_and(|owner| owner != token) {
                    return Err("Another browser has control".into());
                }
                // Disable the bot before releasing the queue, under the same
                // lock that checks every bot command.
                s.bot = false;
                self.1.store(true, Ordering::Relaxed);
                neutral(&mut s)?;
                s.persist(true)?;
                s.owner = Some(token.into());
                s.sequence = 0;
                s.renewed = Instant::now();
            }
            "input" | "release" | "heartbeat" => {
                if !valid_token || s.owner.as_deref() != Some(token) {
                    return Err("Take control first; your control lease may have expired".into());
                }
                if action == "input" {
                    let seq = body["sequence"].as_u64().ok_or("sequence is required")?;
                    if seq <= s.sequence {
                        return Err("Out-of-order input".into());
                    }
                    let buttons: ButtonSet = serde_json::from_value(body["buttons"].clone())
                        .map_err(|e| e.to_string())?;
                    neutral(&mut s)?;
                    if !buttons.is_empty() {
                        s.controller
                            .execute(ControllerCommand::Hold {
                                buttons,
                                duration: HOLD,
                            })
                            .map_err(|e| e.to_string())?;
                    }
                    s.sequence = seq;
                } else if action == "release" {
                    neutral(&mut s)?;
                    s.owner = None;
                }
                s.renewed = Instant::now();
            }
            "shutdown" => {
                let stop = s
                    .shutdown
                    .clone()
                    .ok_or("Only emulator instances can be shut down")?;
                s.bot = false;
                self.1.store(true, Ordering::Relaxed);
                s.owner = None;
                neutral(&mut s)?;
                stop.store(true, Ordering::Relaxed);
            }
            "stop" => {
                s.bot = false;
                self.1.store(true, Ordering::Relaxed);
                s.owner = None;
                neutral(&mut s)?;
                s.persist(true)?;
            }
            "resume" => {
                if !s.resume {
                    return Err("This bot cannot resume yet".into());
                }
                if s.owner.as_deref().is_some_and(|owner| owner != token) {
                    return Err("Another browser has control".into());
                }
                neutral(&mut s)?;
                s.owner = None;
                s.persist(false)?;
                s.resume = false;
                s.bot = true;
                self.1.store(false, Ordering::Relaxed);
            }
            _ => return Err("Unknown control action".into()),
        }
        drop(s);
        Ok(self.status())
    }
}

impl State {
    fn persist(&self, stopped: bool) -> std::result::Result<(), String> {
        if let Some(path) = &self.state_file {
            if stopped {
                std::fs::write(path, b"stopped\n").map_err(|e| e.to_string())?;
            } else if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e.to_string());
                }
            }
        }
        Ok(())
    }

    fn expire(&mut self) {
        if self.owner.is_some() && self.renewed.elapsed() >= LEASE {
            let _ = self.controller.execute(ControllerCommand::Neutral);
            self.owner = None;
        }
    }
}

struct BotController(GameControl);
impl Controller for BotController {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        let mut s = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        if !s.bot {
            return Err(Error::Device("bot stopped by web control".into()));
        }
        s.controller.execute(command)
    }
    fn is_idle(&self) -> Result<bool> {
        self.0
             .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller
            .is_idle()
    }
    fn console_link(&mut self) -> Result<ConsoleLink> {
        self.0
             .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller
            .console_link()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_core::Button;

    struct Fake(Arc<Mutex<Vec<ControllerCommand>>>);
    impl Controller for Fake {
        fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
            self.0.lock().unwrap().push(command);
            Ok(ControllerReceipt {
                command_id: 1,
                issued_at: Instant::now(),
                input_duration: Duration::ZERO,
            })
        }
        fn is_idle(&self) -> Result<bool> {
            Ok(true)
        }
    }
    #[test]
    fn takeover_cancels_queue_and_rejects_bot_and_other_browser() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let control = GameControl::new(Box::new(Fake(commands.clone())), true);
        let mut bot = control.bot_controller();
        bot.execute(ControllerCommand::Press(Button::A)).unwrap();
        control
            .request(json!({"action":"take", "owner":"one"}))
            .unwrap();
        assert!(bot.execute(ControllerCommand::Neutral).is_err());
        assert!(control
            .request(json!({"action":"take", "owner":"two"}))
            .is_err());
        control
            .request(json!({"action":"input", "owner":"one", "sequence":2, "buttons":["Up"]}))
            .unwrap();
        assert!(control
            .request(json!({"action":"input", "owner":"one", "sequence":1, "buttons":[]}))
            .is_err());
        let commands = commands.lock().unwrap();
        assert!(matches!(commands[1], ControllerCommand::Neutral));
        assert!(
            matches!(commands.last(), Some(ControllerCommand::Hold {duration, ..}) if *duration == HOLD)
        );
    }
    #[test]
    fn expired_lease_releases_inputs_without_resuming_bot() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let control = GameControl::new(Box::new(Fake(commands.clone())), true);
        control
            .request(json!({"action":"take", "owner":"one"}))
            .unwrap();
        control.0.lock().unwrap().renewed = Instant::now() - LEASE;
        assert_eq!(control.status()["mode"], "stopped");
        assert!(!control.bot_enabled());
        assert!(matches!(
            commands.lock().unwrap().last(),
            Some(ControllerCommand::Neutral)
        ));
        assert!(control.request(json!({"action":"resume"})).is_err());
        control.allow_resume(true);
        control.request(json!({"action":"resume"})).unwrap();
        assert!(control.bot_enabled());
    }

    #[test]
    fn user_stop_survives_process_restart_until_explicit_resume() {
        let path =
            std::env::temp_dir().join(format!("web-control-test-{}.paused", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let fake = || Box::new(Fake(Arc::new(Mutex::new(Vec::new()))));
        let control = GameControl::new(fake(), true);
        control.persist_stopped_state(path.clone()).unwrap();
        control.request(json!({"action":"stop"})).unwrap();
        drop(control);
        let control = GameControl::new(fake(), true);
        control.persist_stopped_state(path.clone()).unwrap();
        assert!(!control.bot_enabled());
        assert!(control.request(json!({"action":"shutdown"})).is_err());
        control.allow_resume(true);
        control.request(json!({"action":"resume"})).unwrap();
        assert!(control.bot_enabled());
        assert!(!path.exists());
    }
}
