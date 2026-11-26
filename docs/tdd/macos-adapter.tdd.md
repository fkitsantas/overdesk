# TDD evidence: macOS adapter

Source plan: Phase 2 of the README roadmap, with the design in `docs/ARCHITECTURE.md` §4–5.

## User journeys

- As a Mac user, I want my Magic Mouse and trackpad scrolling, including momentum, to arrive intact on the other computer.
- As a Mac user, I want ⌘, ⌥, Fn/🌐, Caps Lock and media keys to work on the other computer, with no stuck keys.
- As the user of a Mac driven from another computer, I want injected input to behave like real hardware: drags, double-clicks, modifier shortcuts.

## Cycles

| # | Behaviour | RED | GREEN |
|---|---|---|---|
| 1 | Key tables, modifier decoding, injection flags, scroll phases, drag types, click counter | `250085c`: 14/15 failed at `todo!()` | `feat: macOS pure translation layer`: 15/15 pass |
| 2 | Review fixes: `LocalDowns` lets locally pressed releases through, `Subpixel` keeps fractional motion | `test: add failing LocalDowns and Subpixel tests`: 5 failed | `2a56a99`: 20/20 pass |

## Hand test on real hardware (`examples/mac_probe`)

| Check | Result |
|---|---|
| Pointer capture: deltas, signs, drags (button held while moving) | ✅ |
| Sub-pixel motion: no empty `Moved {0,0}` after the fix | ✅ |
| Pointer injection: cursor read back at 4 square corners | ✅ 4/4 exact, and seen on screen |
| No feedback loop between capture and injection in one process | ✅ |
| Real deltas after an injected 1000 px jump don't include the jump | ✅ (first real delta 180 px) |
| Scroll phases `Began → Changed → Ended → MomentumBegan → Momentum → MomentumEnded`, pixel units | ✅ |
| Scroll signs: down gives `dy < 0`, left gives `dx < 0` | ✅ |
| ⌘, left and right Shift, Fn/🌐 (`0xFF0003`), Caps Lock (`57`) press and release | ✅ |
| Plain keys (V, Return, A, Tab) | ✅ |
| Double-click: two down/up pairs | ✅ |
| Key repeat (forwarded autorepeat) | ⏳ Not yet observed. A held letter opens the macOS accent picker instead of repeating; retest with an arrow key. |
| Scroll injection glides on screen | ⏳ Not yet confirmed |

## Known gaps

- Posting media keys (needs AppKit system-defined events).
- Gesture injection: no public macOS API.
- Hiding and freezing the local cursor while focus is on another peer.
- The double-click interval is a fixed 500 ms.
- Several displays on one Mac.
