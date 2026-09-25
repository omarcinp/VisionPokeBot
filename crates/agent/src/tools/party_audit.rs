//! Establish the whole roster from visible menus before trusting a save.
use super::menu::{open_start_menu, pick_row, Closer, MenuRow, Retries, SCREEN_FRAMES};
use super::{Expects, StepContext, ToolContext, ToolError, ToolStep};
use crate::{Action, Decision, Expectation, Outcome};
use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Knowledge, MoveSlot, PartyMon, SummaryObservation, SummaryPage};
use std::sync::Arc;

pub fn audit(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    let mut step = PartyAudit {
        data: Arc::clone(&ctx.data),
        retries: Retries::default(),
        closer: Closer::default(),
        count: None,
        menu_candidate: None,
        members: Vec::new(),
        mon: PartyMon::default(),
        page: 0,
        candidate: None,
        returning: false,
        done: false,
    };
    ctx.drive(&mut step).map(|_| ())
}

struct PartyAudit {
    data: Arc<GameData>,
    retries: Retries,
    closer: Closer,
    count: Option<u8>,
    menu_candidate: Option<(u64, pokebot_state::PartyMenuObservation)>,
    members: Vec<PartyMon>,
    mon: PartyMon,
    page: u8,
    candidate: Option<(u64, SummaryObservation)>,
    returning: bool,
    done: bool,
}

impl PartyAudit {
    fn press(&mut self, button: Button, expect: Expectation) -> Decision {
        self.retries
            .act("party audit", button, expect, SCREEN_FRAMES)
    }

    fn read_page(&mut self, s: &SummaryObservation, frame: u64) -> Result<(), String> {
        let level = s.level.ok_or("summary level unreadable")?;
        if s.nickname.is_empty() || s.nickname.contains('?') {
            return Err("summary nickname unreadable".into());
        }
        if self.page > 0 && self.mon.nickname.value.as_ref() != Some(&s.nickname) {
            return Err("summary member changed during audit".into());
        }
        match s.page {
            SummaryPage::Info => {
                let species = s
                    .species
                    .as_deref()
                    .and_then(|s| self.data.species_named(s))
                    .ok_or("summary species unreadable")?;
                let item = match s.held_item.as_deref() {
                    Some("NONE") => None,
                    Some(name) => Some(
                        self.data
                            .item_named(name)
                            .ok_or("held item unreadable")?
                            .to_owned(),
                    ),
                    None => return Err("held item unreadable".into()),
                };
                self.mon = PartyMon {
                    species: Knowledge::observed(species.to_owned(), frame),
                    nickname: Knowledge::observed(s.nickname.clone(), frame),
                    level: Knowledge::observed(level, frame),
                    status: Knowledge::observed(s.status.ok_or("status unreadable")?, frame),
                    held_item: Knowledge::observed(item, frame),
                    ..PartyMon::default()
                };
            }
            SummaryPage::Skills => {
                let hp =
                    s.hp.filter(|&(cur, max)| {
                        cur <= max
                            && (max >= u16::from(level) + 10
                                || (max == 1
                                    && self.mon.species.value.as_deref()
                                        == Some("SPECIES_SHEDINJA")))
                    })
                    .ok_or("summary HP unreadable or impossible")?;
                self.mon.hp = Knowledge::observed(hp, frame);
                if hp.0 == 0 {
                    self.mon.status = Knowledge::observed(pokebot_state::Status::Fainted, frame);
                }
            }
            SummaryPage::Moves => {
                if s.moves.is_empty() || s.moves.len() > 4 {
                    return Err("move list unreadable".into());
                }
                for (i, (name, pp)) in s.moves.iter().enumerate() {
                    let mv = self.data.move_named(name).ok_or("move name unreadable")?;
                    let pp = pp.ok_or("move PP unreadable")?;
                    if pp.0 > pp.1 || pp.1 == 0 {
                        return Err("invalid move PP".into());
                    }
                    self.mon.moves[i] = Some(MoveSlot {
                        mv: Knowledge::observed(mv.to_owned(), frame),
                        pp: Knowledge::observed(pp, frame),
                    });
                }
            }
        }
        Ok(())
    }
}

impl ToolStep for PartyAudit {
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
        if self.done {
            return self
                .closer
                .next(o, "party, HP, status, moves and PP audited");
        }
        if self.retries.exhausted() {
            return Decision::Fail("party audit: screen or field stayed unreadable".into());
        }
        if self.returning {
            if o.party_menu.as_ref().is_some_and(|m| !m.actions) {
                self.returning = false;
                self.page = 0;
                self.candidate = None;
                if self.members.len() == self.count.unwrap_or(0) as usize {
                    ctx.events.push(GameEvent::PartyAudited {
                        members: self.members.clone(),
                    });
                    self.done = true;
                    return self.closer.next(o, "party audited");
                }
            } else {
                return self.press(Button::B, Expectation::PartyList);
            }
        }
        if let Some(s) = &o.summary {
            let wanted =
                [SummaryPage::Info, SummaryPage::Skills, SummaryPage::Moves][self.page as usize];
            if s.page != wanted {
                return self.press(
                    if self.page == 0 {
                        Button::Left
                    } else {
                        Button::Right
                    },
                    Expectation::SummaryPage(wanted),
                );
            }
            if !self
                .candidate
                .as_ref()
                .is_some_and(|(f, prev)| o.frame_id.saturating_sub(*f) >= 15 && prev == s)
            {
                if self.candidate.as_ref().is_none_or(|(_, prev)| prev != s) {
                    self.candidate = Some((o.frame_id, s.clone()));
                }
                return self.retries.wait(o, "confirming summary on a later frame");
            }
            if let Err(why) = self.read_page(s, o.frame_id) {
                return self.retries.wait(o, &why);
            }
            self.candidate = None;
            if self.page == 2 {
                self.members.push(self.mon.clone());
                self.returning = true;
                return self.press(Button::B, Expectation::PartyList);
            }
            self.page += 1;
            return self.press(
                Button::Right,
                Expectation::SummaryPage(
                    [SummaryPage::Info, SummaryPage::Skills, SummaryPage::Moves]
                        [self.page as usize],
                ),
            );
        }
        if let Some(menu) = &o.party_menu {
            if menu.actions {
                return self.press(Button::A, Expectation::SummaryPage(SummaryPage::Info));
            }
            if !self
                .menu_candidate
                .as_ref()
                .is_some_and(|(f, m)| o.frame_id.saturating_sub(*f) >= 15 && m == menu)
            {
                if self.menu_candidate.as_ref().is_none_or(|(_, m)| m != menu) {
                    self.menu_candidate = Some((o.frame_id, menu.clone()));
                }
                return self.retries.wait(o, "letting party panels finish drawing");
            }
            self.count.get_or_insert(menu.count);
            if self.count != Some(menu.count) {
                return Decision::Fail("party size changed during audit".into());
            }
            let target = self.members.len() as u8;
            let Some(selected) = menu.selected else {
                return self.retries.wait(o, "reading party selection");
            };
            if selected != target {
                return self.press(
                    if selected < target {
                        Button::Down
                    } else {
                        Button::Up
                    },
                    Expectation::PartySelected(if selected < target {
                        selected + 1
                    } else {
                        selected - 1
                    }),
                );
            }
            return self.press(Button::A, Expectation::PartyActions);
        }
        if let Some(menu) = &o.menu {
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
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}
