//! Tiny input script format for smoke tests and fixture capture.
//!
//! ```text
//! # comment
//! wait 120                      observe 120 frames
//! press A                       tap a button
//! chord A+B                     tap several buttons together
//! hold Up 250ms                 hold for a duration (ms or s)
//! sequence Up:100ms none:50ms   explicit timeline
//! idle                          observe frames until the controller is idle
//! screenshot captures/x.png     save the current normalized frame
//! ```
//!
//! Frame waits here are a development convenience only; gameplay logic must
//! confirm state from video, never from elapsed frames.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pokebot_core::{ButtonSet, ControllerCommand, TimedInput};

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Wait(u64),
    Command(ControllerCommand),
    UntilIdle,
    Screenshot(PathBuf),
}

pub fn parse(text: &str) -> Result<Vec<Step>> {
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.split('#').next().unwrap_or("").trim();
            (!line.is_empty())
                .then(|| parse_line(line).with_context(|| format!("line {}: {line}", i + 1)))
        })
        .collect()
}

fn parse_line(line: &str) -> Result<Step> {
    let words: Vec<&str> = line.split_whitespace().collect();
    Ok(match words[..] {
        ["wait", frames] => Step::Wait(frames.parse()?),
        ["press", button] => Step::Command(ControllerCommand::Press(button.parse()?)),
        ["chord", buttons] => Step::Command(ControllerCommand::Chord(buttons.parse()?)),
        ["hold", buttons, duration] => Step::Command(ControllerCommand::Hold {
            buttons: buttons.parse()?,
            duration: parse_duration(duration)?,
        }),
        ["sequence", ref inputs @ ..] if !inputs.is_empty() => {
            let timeline = inputs
                .iter()
                .map(|input| {
                    let (buttons, duration) = input
                        .split_once(':')
                        .with_context(|| format!("expected buttons:duration, got {input}"))?;
                    Ok(TimedInput {
                        buttons: buttons.parse::<ButtonSet>()?,
                        duration: parse_duration(duration)?,
                    })
                })
                .collect::<Result<_>>()?;
            Step::Command(ControllerCommand::Sequence(timeline))
        }
        ["neutral"] => Step::Command(ControllerCommand::Neutral),
        ["idle"] => Step::UntilIdle,
        ["screenshot", path] => Step::Screenshot(path.into()),
        _ => bail!("unrecognised step"),
    })
}

fn parse_duration(s: &str) -> Result<Duration> {
    if let Some(ms) = s.strip_suffix("ms") {
        Ok(Duration::from_millis(ms.parse()?))
    } else if let Some(secs) = s.strip_suffix('s') {
        Ok(Duration::from_secs_f64(secs.parse()?))
    } else {
        bail!("duration needs a unit (ms or s): {s}")
    }
}

#[cfg(test)]
mod tests {
    use pokebot_core::Button;

    use super::*;

    #[test]
    fn parses_every_step_kind() {
        let steps = parse(
            "# boot\nwait 10\npress a\nchord A+B\nhold Up 250ms # walk\nsequence up:0.1s none:50ms\nidle\nneutral\nscreenshot out.png\n",
        )
        .unwrap();
        assert_eq!(steps.len(), 8);
        assert_eq!(steps[0], Step::Wait(10));
        assert_eq!(steps[1], Step::Command(ControllerCommand::Press(Button::A)));
        assert_eq!(
            steps[3],
            Step::Command(ControllerCommand::Hold {
                buttons: Button::Up.into(),
                duration: Duration::from_millis(250)
            })
        );
        assert_eq!(steps[5], Step::UntilIdle);
        assert_eq!(steps[7], Step::Screenshot("out.png".into()));
    }

    #[test]
    fn reports_bad_lines() {
        let err = parse("wait 1\nhold A 10\n").unwrap_err();
        assert!(format!("{err:#}").contains("line 2"));
        assert!(parse("jump").is_err());
    }
}
