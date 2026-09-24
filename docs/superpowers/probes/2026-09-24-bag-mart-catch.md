# Probe: bag, mart, catching, Mt. Moon (2026-09-24)

Measured on normalized 240×160 frames from the in-process stepped emulator
(mGBA). The values are exact emulator colours. Capture sources need the usual
±20–24 tolerance. Every coordinate is in normalized frame pixels, and ranges are
inclusive (`x 87..231` means x = 87 through 231). A region written as `(x, y, w, h)`
is in the same form as `pokebot_state::Region::new(x, y, w, h)`.

Fixtures live in `captures/fixtures/` (gitignored). They are produced by the
scripts in `tools/scripts/probe/` (see the README there), replayed against
`saves/route3-ready.sav`. Text regions were checked with `tools/probe/fontread.py`,
a Python port of `Font::read` that gives the same readings as `pokebot inspect`
on dialogue and battle text. "normal" is `data/world/font_normal.json` and
"small" is `data/world/font_small.json`.

**Digit caveat (small font):** `0` and `O` share one bitmap in the small font,
and `Font::from_json` keeps the letter. Prices and counts therefore read as
`¥2OO` or `×O3`, so a numeric field reader must map `O` → `0`. The currency
glyph reads as `¥`. On screen it looks like ₽, but the font calls it `¥`, and
the dialogue text reads `¥600` the same way.

Shared colours:

| Name | RGB | Where |
|---|---|---|
| list ink | (99, 97, 99) | mart/bag list text, field ▶ cursor, YES/NO text |
| list shadow | (214, 211, 206) | text shadow; fill of the inactive ▷ cursor |
| mart list cream | (255, 251, 214) | mart item list interior |
| bag list cream | (255, 251, 206) | bag item list interior (different from the mart) |
| row dash | (239, 227, 173) | dashed underline under every list row (both lists) |
| white | (255, 251, 255) | menu windows, description text ink |
| description blue | (0, 121, 198) | item description panel (mart and bag) |
| battle ▶ / box edge | (41, 48, 49) | battle-style cursor (nickname Yes/No) |
| arrow red | (255, 81, 0) | pocket arrows, list scroll arrow, quantity ▲▼ |

---

## 1. Mart (`PewterCity_Mart`)

**Button path** (from `PewterCity_Mart` (4,7)): walk Up ×4 to (4,3), press Left
(face the clerk, object 3 at (2,3), across the counter at (3,3)), then A →
"Hi, there! / May I help you?" The BUY/SELL/SEE YA! menu then appears with the
text; no extra A is needed. A (BUY) → list. A on POKé BALL → "POKé BALL?
Certainly. / How many would you like?" and the quantity box. Up ×2 → ×03. A →
"POKé BALL, and you want 3. / That will be ¥600. Okay?" with YES/NO. A (YES) →
"Here you are! / Thank you!" → A → back to the list, cursor on the same row.
To leave: B (closes the list, "Is there anything else I can do?", the menu comes
back) → B (= SEE YA!) → "Please come again!" → A. The text prints slowly, so wait
about 240 frames after each A before the next input. Inputs pressed earlier are
lost (an Up pressed before the quantity box appears does nothing).

### `mart-menu.png` — BUY / SELL / SEE YA!
- Window: standard white menu. Interior `x 14..113, y 6..57`. Frame going
  outward: (222,211,222) ×1, (115,105,132) ×2, (140,138,206) or (74,73,107) ×1,
  (41,48,49) ×1. The existing menu detector finds it: "menu 3 rows, cursor 0 at
  (14,6)".
- Rows: text bands at y 13, 29, 45 (pitch 16). Region `(24, 6, 90, 52)`
  (normal) reads `BUY`, `SELL`, `SEE YA!`.
- Cursor: the field ▶ in (99,97,99), top-left (17,12), 9 rows with widths
  1,2,3,4,5,4,3,2,1, and a shadow on its right.
- Dialogue below: the standard message box (interior `x 8..231, y 118..153`).
  Reads "Hi, there!", "May I help you?"

### `mart-list.png` — item list and MONEY window
- **MONEY window:** interior `x 5..74, y 5..34`, white. Frame: (206,211,214) ×1,
  then (99,113,123) ×2. Label: region `(5, 5, 70, 15)` (normal) → `MONEY`, ink
  y 11..18. Amount: region `(5, 20, 70, 15)` (small) → `¥488O` (= 4880), ink
  y 24..30, right-aligned and ending at x 70.
- **Item list window:** outer `x 80..238, y 1..110`. Interior (mart cream
  (255,251,214)) `x 87..231, y 8..103`. Frame going outward: (255,186,132)
  ×3, then (255,219,165) ×2 on the top/left or (222,154,107) ×2 on the
  bottom/right, then (107,105,107) ×2.
- **Rows:** 6 visible, pitch 16. Row *r* has its cell at `y = 8 + 16r`, text
  ink at y 13+16r..20+16r, and a dashed underline (239,227,173) at
  y 23+16r..24+16r (x 97..222, 6-px dashes with 2-px gaps). Use a height of 15
  so the dash stays out of the region:
  - name: `(96, 8 + 16r, 94, 15)` (normal) → `POKé BALL`, `POTION`, `ANTIDOTE`,
    `PARLYZ HEAL`, `AWAKENING`, `BURN HEAL`;
  - price: `(190, 8 + 16r, 42, 15)` (**small**) → `¥2OO`, `¥3OO`, `¥1OO`,
    `¥2OO`, `¥25O`, `¥25O`. The normal font cannot read prices (all `?`).
- **Scroll arrow:** a red ▼ (255,81,0) at `x 154..165, y 102..108`. The list
  continues below BURN HEAL. It bobs, so don't match it exactly.
- **Cursor:** ▶ ink (99,97,99) at `x 90..94, y 12+16r..20+16r`, with its
  shadow (214,211,206) on the right edge. **Inactive ▷** (while the quantity box
  or a dialogue has focus, in `mart-quantity-*` and `mart-confirm`): the same
  shape, but the colours swap. The fill becomes (214,211,206) and only the
  right-hand edge is (99,97,99). The existing gray-▶ detector must not match it.
- **Description panel** (bottom): blue (0,121,198) `x 0..239, y 115..156`. The
  top edge is (16,170,222) at y 114, with (0,81,115) at y 112..113. The item
  icon sits in a white box `x 7..33, y 123..149`. Text: region
  `(38, 115, 200, 45)` (normal, white ink) → `A BALL thrown to catch a wild`,
  `POKéMON. It is designed in a`, `capsule style.`. The line pitch is **15** in
  the mart (ink tops at y 118, 133, 148) but **14** in the bag. With a height
  of 42 the third line's p/y are clipped and read `?`, so use 45.
- **Existing detector:** `pokebot inspect` reports "menu 1 rows, cursor 0 at
  (88,12)". It finds the ▶ but measures the window as white, so its rows and
  window are wrong on the cream list.

### `mart-quantity-1.png` / `mart-quantity-3.png` — quantity box
- **Quantity box:** interior `x 134..233, y 70..105`, white, with the standard
  menu frame. Text: one line, ink y 86..92, x 139..226. Region
  `(134, 80, 100, 16)` (**small**) → `×O1 ¥2OO` / `×O3 ¥6OO`, meaning ×01 ¥200
  and ×03 ¥600 (the total). The normal font reads nothing here.
- **▲ / ▼** (255,81,0): ▲ at `x 146..157, y 67..73`, ▼ at `x 146..157,
  y 102..108`. They sit on the box's top and bottom frame.
- **IN BAG window:** interior `x 5..114, y 85..106`, white, with the MONEY-style
  frame ((206,211,214) ×1, (99,113,123) ×2). Label: region `(5, 85, 50, 22)`
  (normal) → `IN BAG:`. Count: region `(55, 85, 60, 22)` (**small**) → `5`,
  ink x 64..68, y 94..100. Reading both at once as normal gives `IN BAG: ?`.
- Dialogue: "POKé BALL? Certainly." / "How many would you like?" (standard box).

### `mart-confirm.png` — "That will be ¥600. Okay?"
- **YES/NO window:** interior `x 166..217, y 70..105`, standard menu frame. Text
  region `(176, 70, 42, 36)` (normal) → `YES`, `NO`. The ink bands are at
  y 77..84 and y 91..98, so the **pitch is 14** here (not 16). ▶ (99,97,99) at
  top-left (169,76).
- Dialogue: `POKé BALL, and you want 3.` / `That will be ¥600. Okay?`
- After YES: MONEY shows `¥428O` and the dialogue reads "Here you are! / Thank
  you!". Buying 1 POTION the same way gives "That will be ¥300. Okay?" and
  ¥3980 afterwards.

---

## 2. Field bag (Start → BAG)

**Button path** (overworld): Start (the start menu opens on POKéDEX, rows
POKéDEX / POKéMON / BAG / RED / SAVE / OPTION / EXIT) → Down ×2 → A. The bag
opens on the ITEMS pocket (first open) → Right → KEY ITEMS → Right → POKé BALLS
→ Down. B closes the bag straight back to the start menu (the cursor stays on
BAG). B again closes the start menu.

### `bag-items.png`, `bag-pokeballs.png`, `bag-pokeballs-cursor1.png`
- **Outer bag frame:** orange (247,203,115) `x 6..236, y 4..107`, outlined in
  (107,105,107). Teal striped background (107,203,198) / (66,178,165).
- **Pocket title plate:** plate (247,203,115). Its underline (222,138,74) is at
  `x 8..79, y 19..23`. Title: region `(8, 4, 76, 16)` (normal, white ink
  (255,251,255)) → `ITEMS` / `KEY ITEMS` / `POKé BALLS`. The ink sits in
  y 12..19. It is centred: POKé BALLS spans x 14..72 and ITEMS spans x 29..57.
- **Pocket arrows** (255,81,0), 7×12. The left ◀ is at `x 4..10, y 66..77`
  (present when there is a pocket to the left) and the right ▶ at
  `x 70..76, y 66..77`. ITEMS shows only ▶, KEY ITEMS shows both, and POKé
  BALLS shows only ◀ (it is the last pocket). They bob by a pixel or two.
- **Item list:** interior (bag cream (255,251,206)) `x 88..231, y 8..102`.
  Frame going outward: (214,178,82) ×2, (247,203,115) ×2–3, (107,105,107) ×2.
  Row geometry is the same as the mart: 6 rows, pitch 16, row *r* cell at
  `y = 8 + 16r`, dashed underline at y 23+16r..24+16r (also at y 3).
  - name: `(96, 8 + 16r, 94, 15)` (normal) → `POTION` / `CANCEL` (ITEMS);
    `POKé BALL` / `CANCEL` (POKé BALLS);
  - count: `(190, 8 + 16r, 42, 15)` (**small**) → `× 1`, `× 8` (the `×` is at
    x ≈ 200 and the digits end at x 219). CANCEL has no count.
  - The last row of every pocket is `CANCEL`. An empty pocket shows only CANCEL
    on row 0 (ITEMS before the probe bought a POTION).
- **Cursor:** ▶ (99,97,99) at `x 90..94, y 12+16r..20+16r`.
  `bag-pokeballs-cursor1` has it on row 1 (CANCEL).
- **Description panel:** blue (0,121,198) `x 0..239, y 115..156`, icon box
  `x 7..33, y 123..149`. Text region `(38, 115, 200, 45)` (normal, white). Line
  pitch 14 (ink tops at y 118, 132, 146). It reads `A spray-type wound
  medicine.` / … (POTION), the POKé BALL text, or `CLOSE BAG` on CANCEL.
- The existing detector reports "menu 1 rows, cursor 0 at (88,12)". The ▶ is
  found, but the window and rows are wrong.

### `start-menu.png`, `start-menu-bag.png` (Task 7)
- **Start menu:** the menu detector finds the window at `(174, 6)`, height
  108 (7 rows at a 15 px pitch). The ▶ is at `y = 10 + 15r` (4 px below its
  row). The font reads the window, with the ▶ cell left out, as `POKéDEX`,
  `POKéMON`, `BAG`, `RED`, `SAVE`, `OPTION`, `EXIT`
  (`Observation.menu_lines`). The ▶ row is `(cursor_y − window.y) / 15`.
- The blue help line under the Start menu (`Equipped with pockets…` on BAG)
  is **not** detected as dialogue.
- The menu is drawn over 2 frames: the first ones show "menu 1 rows" with no
  full reading. The same happens as the bag fades in (the list ▶ shows as a
  1-row menu at (88,12) for 2 frames). While the pocket slides, the title
  and rows read empty with no ▶.

---

## 3. Battle: bag, throw, catch

Species and levels: **the first encounter is a wild RATTATA ♀ Lv3, not caught
before** (`battle-wild-uncaught`, `bag-use-prompt`, `battle-throw`,
`battle-broke-free`, `battle-gotcha`, `pokedex-page`, `nickname-prompt`). **The
second is a wild RATTATA ♂ Lv3, after the catch** (`battle-wild-caught`). Our
lead is IVYSAUR ♂ Lv18, with 54/54 HP and then 51/54. Both encounters are in
the Route 2 grass just south of Pewter, at `Route2` (8,3)/(6,3) (see the
Surprises).

**Button path** (at "What will IVYSAUR do?", cursor on FIGHT): Right (→ BAG) →
A → the battle bag opens **on the pocket and row last used in the field bag**
(POKé BALLS, row 1 CANCEL, in this replay) → Up (→ POKé BALL) → A → the USE
prompt → A (USE) → "RED used / POKé BALL!" → throw, shakes → the result text.
After "broke free" comes A → the wild mon's move → back to the command menu, and
**the command cursor stays on BAG**. The bag reopens on POKé BALL (row 0), so the
second throw is A, A, A. After "Gotcha!": A → "RATTATA's data was added to the
POKéDEX." ▼ → A → Pokédex intro (sprite on a circle, "No019 RATTATA" label,
~80 frames) → the Pokédex page → A → a transition → "Give a nickname to the
captured RATTATA?" with Yes/No → B (= No) → overworld.

### `battle-wild-uncaught.png` / `battle-wild-caught.png` — command menu and HUD
- **Opponent HUD:** interior (255,251,222) `x 16..101, y 19..38`. The dark edge
  (33,56,0) bounds `x 14..103, y 17..40`. Name, gender and level are read by the
  existing HUD reader (`RATTATA`, Lv3).
- **Caught-ball icon:** a 7×7 Poké Ball at `x 20..26, y 31..37`, left of the HP
  bar and under the name. It is absent (all HUD background (255,251,222)) when
  the species is not caught. Pixel map (rows y 31..37, columns x 20..26):

  ```
  y31  ..###..      # (74,65,90)  outline
  y32  .#s+*#.      s (255,251,255) highlight
  y33  #+++**#      + (255,178,66)  top half, orange
  y34  #===###      * (222,105,90)  top half, red
  y35  #ss%%%#      = (132,130,140) band
  y36  .#%%%#.      % (214,203,189) bottom half
  y37  ..###..
  ```
  Detector: "(74,65,90) outline plus (255,178,66) or (222,105,90) inside `x 20..26,
  y 31..37`" versus "all (255,251,222)". Within the opponent HUD, this was the only
  difference between the two fixtures apart from the gender symbol (♀ red vs
  ♂ blue).
- **Wild mon front sprite:** RATTATA's ink spans `x 155..190, y 37..71` (the
  diff against `battle-gotcha`, where the platform is empty). The game's
  64×64 opponent sprite box is `x 144..207, y 8..71` (centre (176,40), bottom
  on the platform at y 71). Use this box for shiny palette checks. Species
  sprites have different y offsets inside it.
- Command menu and battle text: existing detectors. `BattleCommand`, cursor
  column 0 row 0.

### `bag-use-prompt.png` — USE / CANCEL over the battle bag
- The top part (y < 112) is **pixel-identical to the field bag** (the frame,
  title `POKé BALLS`, list, `× 8`). It differs only in the list cursor, which
  becomes the inactive ▷, and in the bobbing pocket arrow. The same list regions
  apply.
- **Message window** (bottom left): interior white `x 45..162, y 117..154`.
  Frame (206,211,214) ×1, then (99,113,123) ×2. Text region `(46, 118, 116, 38)`
  (normal, ink (99,97,99)) → `POKé BALL is`, `selected.`
- **USE / CANCEL window:** interior `x 174..233, y 118..153`, standard menu
  frame. Only two options in battle (there is no GIVE/TOSS). Text region
  `(182, 118, 50, 36)` (normal) → `USE`, `CANCEL`. Ink bands y 125..132 and
  141..148, pitch 16. ▶ (99,97,99) top-left (177,124). The existing detector
  sees "menu 2 rows, cursor 0 at (174,118)".
- While the prompt is open, the left part of the bottom panel is dark blue
  (24,81,181) at `x 0..42, y 114..157`, around the icon box.

### Battle text fixtures (the existing battle text box; region as in `vision::text`)
Battle message box interior (41,81,107) `x 8..231, y 119..152`, white ink.
- `battle-throw.png`: `RED used` / `POKé BALL!`, taken 150 frames after USE.
  The ball is in flight or on the ground.
- `battle-broke-free.png`: `Oh, no!` / `The POKéMON broke free!` (first throw).
- Other broke-free texts, captured as extra fixtures (same battle, second throw
  on different frames; see the README):
  `battle-broke-free-shoot.png` `Shoot!` / `It was so close, too!`;
  `battle-broke-free-aargh.png` `Aargh!` / `Almost had it!`;
  `battle-broke-free-aww.png` `Aww!` / `It appeared to be caught!`.
  All four read cleanly with the existing reader.
- `battle-gotcha.png`: `Gotcha!` / `RATTATA was caught!` with ▼ (the HUD is
  still shown and the platform is empty). The ▼ appears only after the catch
  fanfare, about 540 frames after the throw starts. An A pressed earlier is
  ignored.
- Next box: `RATTATA's data was` / `added to the POKéDEX.` ▼. It appeared
  because RATTATA was new to the Pokédex, and the Pokédex page followed.

### `pokedex-page.png` — Pokédex entry after the first catch
- Background tan (198,178,140). **Title bar** `y 0..15`: region `(60, 2, 120, 14)`
  (normal, white) → `Grassland POKéMON` (the habitat).
- **Upper page:** white interior `x 3..236, y 19..87`. Frame (198,178,140) ×2,
  then (231,219,198). Black ink (0,0,0):
  - number: region `(24, 30, 18, 20)` (**small**) → `O19` (= 019). The `№`
    glyph at x ≈ 16..23 does not read, so start the region at x 24;
  - name: region `(40, 30, 60, 20)` (normal) → `RATTATA`;
  - category: region `(14, 48, 110, 14)` (small) → `MOUSE POKéMON`;
  - height: region `(14, 60, 86, 12)` (small) → `HT 1’OO”`;
  - weight: region `(14, 72, 86, 12)` (small) → `WT 7.7 lbs.`;
  - footprint: black, at `x 109..113, y 69..73`. Keep it out of the HT/WT
    regions (at width 110 it reads `?`);
  - sprite: `x 163..198, y 39..73`.
- A divider (123,97,57) runs at y 89..90.
- **Lower page:** (231,219,198) `x 3..236, y 92..140`. Region `(12, 94, 220, 46)`
  (normal, black) → `Its fangs are long and very sharp.` / `They grow
  continuously, so it gnaws on` / `hard things to whittle them down.`
- "Ⓐ NEXT" button hint, white, in `x 180..239, y 142..157`.
- `pokebot inspect` reports `Unknown` for this screen.

### `nickname-prompt.png` — "Give a nickname to the captured RATTATA?"
- The battle background, with the caught mon's sprite alone (no HUDs).
- Battle message box (as above) → `Give a nickname to the` / `captured RATTATA?`
- **Yes/No window:** interior white `x 190..233, y 70..105`, standard menu frame.
  **Mixed case** `Yes` / `No` (the field YES/NO is upper case). Ink (74,73,74).
  Region `(200, 70, 30, 36)` → `Yes`, `No`. Bands at y 77..84 and 93..100, pitch
  16. **Battle-style ▶** (41,48,49) at top-left (193,76), widths 2,3,4,5,5,5,4,3,2.
  The existing detector sees "menu 2 rows, cursor 0 at (190,70)".
- B answers No and returns to the overworld. Down then A (No) was not tried.

---

## 4. Mt. Moon (`MtMoon_1F`)

**Button path** (a separate replay, `mtmoon.txt`): Pewter PC → Pewter east exit.
**Prof. Oak's AIDE stops us at Pewter (46,21)** and gives the RUNNING SHOES, then
Mom's letter follows (~38 text boxes, pressing A every 60 frames). Then onto
Route 3 at (0,11) → (13,10) → (13,8) → (14,7) → (14,6), where BUG CATCHER COLTON
(12,6) sees us → battle. Then up to row 3 above BUG CATCHER BEN, → (24,5) →
step right into BUG CATCHER GREG's sight (25,4) → battle. Then row 5 east to
(36,5) → down into BUG CATCHER JAMES's sight (32,6) → battle. Then (36,9) →
(46,9) → (46,7) → (60,7) → (60,11) → (69,11) → (69,10) → (72,10) → up the
x = 72 column to `Route4` (12,19) → (12,6) → (19,6) → door (19,5) →
`MtMoon_1F` (18,37). The battles are won by pressing A (FIGHT → TACKLE), then B
to close the texts. No wild encounters happened on this path.

- `mtmoon-entry-intro.png`: **on first entry, a full-screen "MT. MOON" location
  intro is shown**. It is a landscape picture of the cave mouth with a "MT. MOON"
  label at top-left, over a black background, about 150 frames after the door.
  The player cannot move yet. It is gone about 450 frames after entry.
- `mtmoon-1f.png`: overworld, player at `MtMoon_1F` (18,35). This is the normal
  lit cave palette (no Flash needed on 1F). The localizer finds it with score
  997.
- `mtmoon-1f-b.png`: the player at (18,33). An item ball is visible to the
  left.
- The whole 1F floor is encounter tiles (every walkable tile has
  `encounter = 1` in the world model).

---

## Surprises (differences from the spec's flow)

1. **The bag remembers pocket *and* row across field and battle.** The battle
   bag opened on POKé BALLS with the cursor on CANCEL (row 1), because the
   field-bag probe had left it there. The battle's BAG therefore cannot assume
   ITEMS or row 0. It must read the pocket title and the ▶ row. The command
   menu also keeps BAG selected after a throw within the same battle. A new
   battle starts on FIGHT.
2. **The battle bag is the field bag.** The layout, colours and regions are
   identical, and only the bottom panel differs (a USE/CANCEL window plus a
   message window instead of the description). There are two options (USE,
   CANCEL), not "USE/…/CANCEL".
3. **Inactive ▷ cursor.** While a sub-window has focus, the list cursor turns
   into an outline with swapped colours (see §1). A detector that looks for the
   gray ▶ finds none in the list. That is correct, but it must not treat the ▷
   as absent-with-a-guess.
4. **Mart YES/NO row pitch is 14**, not 16. The mart description line pitch is
   15, the bag's is 14.
5. **Prices, counts, money, IN BAG and the Pokédex number use the small font**,
   with `O` for `0`. The normal font reads names and labels.
6. **The quantity box needs the dialogue to finish** before Up/Down count. An
   early Up is dropped, and the buy then goes through at ×01.
7. **Buying a POTION:** the Pewter list is POKé BALL, POTION, ANTIDOTE,
   PARLYZ HEAL, AWAKENING, BURN HEAL, and more below (▼). This probe did not
   buy 10+ balls, so the PREMIER BALL bonus page was **not observed**.
8. **The route3-ready bag:** 5 POKé BALLs, an empty ITEMS pocket (no POTIONs),
   ¥4880, and KEY ITEMS TEACHY TV and TM CASE.
9. **Route 3 is not reachable without trainer battles**, and Route 3 grass
   (x 33..46) sits behind them. The catch probe therefore used the **Route 2
   grass right below Pewter** (`Route2` x 2..8, y 2..8, entered from Pewter
   (22,39)). It has no trainers and RATTATA Lv3 appears quickly. On the way to
   Mt. Moon, COLTON (12,6, sight 3), GREG (25,4, sight 2) and JAMES (32,6,
   sight 4) cannot be avoided. JANICE, BEN, SALLY and CALVIN can be. ROBIN, who
   wanders in the grass, was not triggered.
10. **A RUNNING SHOES cutscene at the Pewter east exit** (Oak's AIDE, then
    Mom's letter). It still hasn't played in the route3-ready save, so the
    first walk east from Pewter stops for it. The Route 3 milestone must page
    through it.
11. **The first entry to Mt. Moon shows a full-screen location intro** before
    control returns.
12. **Catch timing:** "Gotcha!" is fully printed about 540 frames after the
    throw starts, and the Pokédex data box, Pokédex page and nickname prompt
    follow. The Pokédex page needs an A to leave. The Yes/No nickname prompt
    uses battle-style colours and mixed case.

## Fixture index

| Fixture | Shows | Script |
|---|---|---|
| `mart-menu.png` | BUY/SELL/SEE YA! + "May I help you?" | `mart.txt` |
| `mart-list.png` | list (▶ POKé BALL), MONEY ¥4880, description | `mart.txt` |
| `mart-quantity-1.png` | ×01 ¥200, IN BAG 5, inactive ▷ | `mart.txt` |
| `mart-quantity-3.png` | ×03 ¥600 | `mart.txt` |
| `mart-confirm.png` | "…want 3. That will be ¥600. Okay?" YES/NO | `mart.txt` |
| `bag-items.png` | ITEMS: POTION ×1, CANCEL (▶ row 0) | `field-bag.txt` |
| `bag-pokeballs.png` | POKé BALLS: POKé BALL ×8, CANCEL (▶ row 0) | `field-bag.txt` |
| `bag-pokeballs-cursor1.png` | same, ▶ on row 1 (CANCEL), "CLOSE BAG" | `field-bag.txt` |
| `bag-use-prompt.png` | battle bag, "POKé BALL is selected." USE/CANCEL | `battle-catch.txt` |
| `battle-wild-uncaught.png` | RATTATA ♀ Lv3, no ball icon, command menu | `battle-catch.txt` |
| `battle-throw.png` | "RED used / POKé BALL!" | `battle-catch.txt` |
| `battle-broke-free.png` | "Oh, no! / The POKéMON broke free!" | `battle-catch.txt` |
| `battle-broke-free-{shoot,aargh,aww}.png` | the other three broke-free texts | variants of `battle-catch.txt` (README) |
| `battle-gotcha.png` | "Gotcha! / RATTATA was caught!" ▼ | `battle-catch.txt` |
| `pokedex-page.png` | No019 RATTATA page | `battle-catch.txt` |
| `nickname-prompt.png` | "Give a nickname to the captured RATTATA?" Yes/No | `battle-catch.txt` |
| `battle-wild-caught.png` | RATTATA ♂ Lv3 with the caught-ball icon, command menu | `battle-catch.txt` |
| `mtmoon-entry-intro.png` | first-entry MT. MOON intro | `mtmoon.txt` |
| `mtmoon-1f.png` | `MtMoon_1F` (18,35) overworld | `mtmoon.txt` |
| `mtmoon-1f-b.png` | `MtMoon_1F` (18,33) overworld | `mtmoon.txt` |

---

## Switch captures (2026-09-24, Task 3b)

Captured on the physical Switch (standalone FireRed, Hagibis MS2109 capture,
`--viewport 180,5,1560,1040 --card-controls switch`, ESP32 controller). The
frames are the runtime's normalized 240×160 frames. They are saved as
`captures/fixtures/switch/<same name as the emulator fixture>.png` (gitignored).

**Game state:** Switch save at `BeatBrock` (`PewterCity_Gym` (6,6)), with RED,
BULBASAUR ♂ Lv14 at 32/37 HP, ¥4880, 5 POKé BALLs, and a Pokédex of 1. The
mart and bag values are the same as in the emulator's route3-ready save (¥4880,
IN BAG 5, POKé BALL ×8 after buying 3, POTION ×1), so the mart and bag
assertions are identical. **The wild species is PIDGEY, not RATTATA:** PIDGEY ♂
Lv3 (uncaught), then PIDGEY ♀ Lv3 (caught), both at `Route2` (7..10,3). The
battle, catch and Pokédex assertions use PIDGEY: `No016 PIDGEY`, "Forest
POKéMON", and `TINY BIRD POKéMON`. The game was soft-reset afterwards, and the
cartridge save was unchanged (CONTINUE: TIME 0:49, POKéDEX 1).

**Driving:** each step was a short `run --script` through `tools/live-run.sh
--instance switch`, with generous waits and a `screenshot`. The position was
checked with `pokebot inspect --world data/world --map <Map>` between steps.
Walking used held directions of about 267 ms per tile plus 120 ms, re-planned
after each segment from the located position (BFS over the world model's
collision and ledges). The mart and bag paths are the emulator's, with waits of
about 1.5× (300 frames after A for mart texts). None of the button paths
differed from the emulator's.

| Fixture | Shows |
|---|---|
| `mart-menu.png` | BUY/SELL/SEE YA! + "Hi, there! / May I help you?" |
| `mart-list.png` | list (▶ POKé BALL), MONEY ¥4880 |
| `mart-quantity-1.png` / `mart-quantity-3.png` | ×01 ¥200 / ×03 ¥600, IN BAG 5 |
| `mart-confirm.png` | "POKé BALL, and you want 3. / That will be ¥600. Okay?" YES/NO |
| `bag-items.png` | ITEMS: POTION ×1, CANCEL (▶ row 0) |
| `bag-pokeballs.png` / `bag-pokeballs-cursor1.png` | POKé BALL ×8, CANCEL, ▶ row 0 / row 1 |
| `bag-use-prompt.png` | battle bag, "POKé BALL is selected.", USE/CANCEL |
| `battle-wild-uncaught.png` | PIDGEY ♂ Lv3, no ball icon, command menu |
| `battle-throw.png` | "RED used / POKé BALL!" |
| `battle-broke-free-aww.png` | "Aww! / It appeared to be caught!" |
| `battle-gotcha.png` | "Gotcha! / PIDGEY was caught!" ▼, empty platform |
| `pokedex-page.png` | No016 PIDGEY page |
| `nickname-prompt.png` | "Give a nickname to the / captured PIDGEY?" Yes/No |
| `battle-wild-caught.png` | PIDGEY ♀ Lv3 with the caught-ball icon, command menu |

**Missing:** `battle-broke-free.png` ("Oh, no!", 0 shakes),
`battle-broke-free-aargh.png` and `battle-broke-free-shoot.png`. Eight throws
gave four "Shoot!", one "Aww!" and three "Gotcha!". The only "Shoot!" frame
kept was taken while the text was still printing ("It was so"), so it was
discarded. The Mt. Moon fixtures were out of scope.

**Colour offsets (Switch − mGBA).** These were measured on pixels whose four
neighbours share the emulator colour, in same-content windows (mart, bag,
prompt) and in reference-colour areas (HUD, battle box, Pokédex):

| Colour | mGBA | mean offset (R,G,B) | max abs |
|---|---|---|---|
| white | (255,251,255) | (0, +4, 0) | 18 |
| mart cream | (255,251,214) | (0, +4, −1) | 11 |
| bag cream | (255,251,206) | (0, +4, −1) | 8 |
| bag orange | (247,203,115) | (0, +6, −3) | 12 |
| description blue | (0,121,198) | (0, +4, 0) | 28 |
| list ink | (99,97,99) | (+1, +3, 0) | 13 |
| arrow red | (255,81,0) | (0, +1, 0) | 9 |
| HUD cream | (255,251,222) | (0, +4, 0) | 12 |
| battle box | (41,81,107) | (−3, +3, +1) | 18 |
| Pokédex tan | (198,178,140) | (−2, +5, −2) | 20 |
| Pokédex lower | (231,219,198) | (−1, +6, −3) | 20 |
| black | (0,0,0) | (0, 0, +1) | 20 |

Over whole windows, the per-pixel max-channel difference has a median of 4, a
p95 of 7–12 and a p99 of 10–17. The maximum is 19–37, at glyph and frame edges
(chroma blur). There is no geometric offset: windows, rows, cursors and the
caught icon sit on the same pixels as in mGBA.
The only content differences are the bobbing ▲▼/◀ arrows. The caught icon's
outline reads (64..87, 57..76, 65..108) and its top half reads (255,179..184,51..74)
and (222..227,104..115,74..86). Detector margins on the Switch: icon outline
209‰ (threshold 120‰), top 86‰ (threshold 50‰). The shiny matcher gets
n = 385 normal-only and s = 15 shiny-only pixels for PIDGEY (Normal needs n ≥ 40
and n ≥ 4·s).

**Readers:** the bag, mart, caught-icon, shiny and Pokédex-page readers passed
on the Switch frames unchanged. Tolerances, share thresholds and alignment did
not need to change. One pre-existing bug surfaced, on **both** sources: the
battle text box was read through the field message box's text region
`(8, 118, 224, 36)`. Its top and bottom rows (y 118, y 153) are the battle
box's frame (231,219,231). Those 448 frame pixels outnumber the text, so the
ink cluster centres on the frame colour and the second line scores out: only
"RED used", "Gotcha!" and "Delete a move to make" were read. The battle text
now uses its interior `BATTLE_TEXT = (8, 119, 224, 34)` (x 8..231, y 119..152),
which reads both lines on the emulator and Switch fixtures, for example
"Gotcha! / PIDGEY was caught!" and "Delete a move to make / room for
POISONPOWDER?".

**Switch timing notes:** in a wild battle, the broke-free text stays up for about
60 frames. The wild mon's move then follows without an A press. A throw's
result text is fully printed 280–360 frames after the A on USE (the number of
shakes varies), and the "Gotcha!" ▼ appears about 460 frames after USE. The Pokédex page
was up 300 frames after the A on "…data was added to the POKéDEX."
