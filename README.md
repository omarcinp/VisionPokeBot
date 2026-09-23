# VisionPokeBot

Vision-driven Pokémon bot. Plays **Fire Red** using **screen capture + controller input only** — it sees the game like a human player and presses buttons like one.

## Design principles

- **Vision-only perception.** The bot NEVER reads emulator memory (no RAM peeks, no save-state inspection). MVP runs on an emulator for convenience, but perception is restricted to framebuffer screenshots — exactly what a capture card would deliver.
- **Controller-only actuation.** Output is a standard gamepad event stream (d-pad, A/B/Start/Select). Same stream drives the emulator today and a USB/Switch controller adapter tomorrow.
- **Hardware path.** End goal: Switch console video output (HDMI capture card) + controller adapter. The `vision/` and `input/` interfaces are hardware-agnostic so swapping emulator → real console touches only drivers, not the agent.

## Layout

- `src/visionpokebot/vision/` — screen capture + frame preprocessing (capture card or emulator framebuffer)
- `src/visionpokebot/input/` — gamepad event stream (virtual gamepad now, USB adapter later)
- `src/visionpokebot/emulator/` — emulator driver (headless mGBA/PyBoy), exposes ONLY framebuffer + joypad input. Memory-debug APIs must stay disabled/unused.
- `src/visionpokebot/agent/` — perception → decision loop (dialog advance, navigation, battle policy)
- `captures/` — local screenshots/video for debugging (gitignored)
- `docs/` — architecture notes, Fire Red milestones

## MVP roadmap

1. Emulator boots Fire Red headless, streams frames, accepts gamepad events
2. Observe/orient/decide/act loop on vision alone (dialog, overworld, battle states)
3. Pallet Town → first rival → Oak parcel → Brock (milestone saves as battery saves, not savestates)
4. Swap emulator driver for HDMI capture + controller adapter → play on real Switch

## Rules

- No ROMs in this repo. Bring your own Fire Red dump; path goes in env var (`VPB_ROM`), never committed.
- No memory reads. Any PR touching emulator debug/memory APIs is rejected by design.
