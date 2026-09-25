# Autonomy: facts, scheduling, recovery and motion

Implemented on the `autonomy` worktree after the September 25 run review.
The campaign itself is still in progress; these changes address its execution
and knowledge failures, rather than completing the remaining story tooling.

## Facts before the first plan

After CONTINUE or the new-game opening, `goal` audits every party member's
INFO, SKILLS and MOVES pages. It reads species, nickname, level, held item,
HP, status, moves and PP, then publishes one atomic `PartyAudited` event.
This replaces the roster, including stale slots from a different save.
Readings must stabilize; unreadable fields stop the audit before planning.

The Trainer Card establishes money and badge flags. Every bag pocket is
audited, including opening the TM Case and Berry Pouch from KEY ITEMS when
present. A container absent from the audited KEY ITEMS list has no contents.
The new-game scripted opening still precedes the first autonomous goal plan;
the menus cannot be audited before a party and field controls exist.

The initial Bulbasaur failure had two causes: low health was not a planning
priority, and Switch HUD OCR read `9/21` as `9/2`. HUD ink tolerance and HP
validation now reject that corrupted total. Old checkpoints are sanitized.
Healing no longer invents an HP maximum: the party summaries verify recovery.
The Switch summary reader also needed a fixed dark-ink numeric mask to read
`27/27` reliably on its pale yellow panel.

## Event handling and suspended work

The runtime reduces events first. The scheduler then recomputes its needs:

| Need | Handling |
| --- | --- |
| Unknown party health, status or PP | Audit before continuing |
| No usable battler, no attacking PP, critically low lead HP | Urgent recovery at the next safe overworld boundary |
| Sole usable battler below 50%, or with an ailment | Urgent recovery |
| Lead below 35% with usable backups | Urgent recovery |
| Other HP below 75%, ailments or low attacking PP | Keep a healing need queued for a cheap detour |
| Battle ended | Reuse the last battle's observed HP, status and tracked PP; audit only missing or inconsistent facts |
| An event contradicts an active plan assumption | Return a replan signal without marking the tool infeasible |

Repeated health events coalesce. Recovery runs with the original tool and
its step machine suspended on the call stack. Successful recovery resumes
that same work; it does not consume a campaign replan. Recovery suppresses
its own safety guard to avoid recursive healing. Movement and battle
interrupts retain their existing action-level cancellation mechanisms.
Needs are reconstructed from fresh observations after a restart.
Battles update the active member without discarding known inactive members.
A normal battle does not trigger a menu tour. Newly caught members with
unknown health still require observation before estimating party risk.

## Choosing recovery

The scheduler considers up to eight nearby healers within five map hops,
including Mom. It prices gated tile routes to actual interaction positions.
Unknown route requirements are treated as blocked for recovery.

For urgent recovery, the score includes outbound travel and encounter
exposure. For a nonurgent stop, travel is the added route cost:

`current → healer → current destination − current → current destination`

Service time and consumable replacement cost contribute to the score.
Already-held Potion, Super Potion, Hyper Potion and Max Potion are candidates
only when one use resolves the health need; they cannot conceal an ailment
or missing PP. Item price is converted at 10 money units per second of
avoided travel. Nonurgent detours have a 20-second additional-cost budget.
Medicine use is verified by a fresh inventory count and increased party HP.

Encounter exposure uses walk length and encounter-tile density on each map.
It is a heuristic, not a measured encounter probability or a guarantee of
globally optimal recovery. The shortlist and conversion weights are explicit
policy choices. Items are used in the field; battle item selection and
switching to another party member remain separate future work.

Wild fights now also compare a conservative two-attack faint risk against
the existing 2% risk limit. High-risk ordinary encounters can RUN even with
a healthy HP percentage, and failed escape attempts may be retried. Trainer
battles still cannot be fled. Visible wild/trainer introduction text corrects
the initial classification based on preceding dialogue.

## Navigation failure and evidence

Run 6's walking estimate had drifted to 1089 ms per tile, with 356 ms of
spread. It issued directional holds of 10–13 seconds and repeatedly passed
the intended Viridian tiles. Arrival timing had included waiting for the
controller to finish, and uncertainty was added to hold duration.

Walking holds now exclude timing spread, reject implausible timing samples,
discard corrupted persisted estimates, use at most four tiles per hold,
and release when the target becomes visible. Timing is measured to visible
arrival independently of controller idle.

- Switch run 7 reached Viridian's Center, healed and resumed training; it
  progressed to level 9. This verifies the previous Viridian loop fix.
- Emulator audit run 3 read the complete roster, Trainer Card, ordinary bag
  and TM Case, then completed `At(PewterCity)` with no execution failures.
- Switch run 8 deliberately stopped before planning when summary HP could
  not be read. Its frame became `switch-summary-skills-27.png`.
- Switch run 9 successfully audited level-9 Bulbasaur at 27/27, moves/PP,
  money 3000, badges and bag contents before campaign planning.
- Its repeated escapes exposed an opponent-HUD crop that included the right
  border as a trailing unknown glyph after the level. Excluding that border
  restores the level-3 Mankey reading and avoids unnecessary unknown-risk RUNs.
- Switch run 10 passed the startup audit with battle-end audits removed, but
  full-campaign planning exceeded its 120-second budget (540 nodes, 125 s).
  Run 11 uses a 240-second budget and produced the campaign plan in about
  60 seconds. Planning latency remains separate work.
- Run 11 reached Route 22 and won its first fight, recording two move uses
  and retaining the observed 27/27 HP. At frame 10482 `BattleEnded` was
  followed by walking and another fight, with no party-menu audit. Both
  encounters chose FIGHT, confirming the unnecessary-escape fix in live play.

Validation: `cargo fmt --all -- --check`, Clippy across the workspace and all
targets with warnings denied, and `cargo test --workspace` passed. The test
run reported 530 passed, 3 ignored, and no failures.

Fixtures and saves remain local and ignored by git. Tests cover corrupted
HP, summary fades, nurse-dialogue false positives, nested bag menus, urgency,
coalescing, invalidated assumptions, medicine eligibility, Mom's route cost,
movement timing and suspension/resumption of the same tool. Full campaign
completion and every possible party/status/menu combination are not implied
by these checks. HM teaching and PC-box tooling remain unfinished elsewhere
on the branch.
