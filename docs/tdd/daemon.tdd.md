# TDD evidence: Desk engine and daemon

Source plan: the advisor-reviewed daemon plan of 2026-11-26. Its first priority was that a
failure must never lock the user out of their Mac.

## Cycles, all RED then GREEN

| Behaviour | Tests |
|---|---|
| Escape chord ⌃⌥⌘⎋ detected in the capture layer; a dead receiver stops swallowing | `input::tests` (2) |
| Split links into reader and writer halves; only trusted links split; peer loss noticed within 3 s | `loopback::split_*`, `only_trusted_links_split` |
| Engine Local / Driving / Driven transition table | `engine::tests` (13) |
| Identity and pinned peers persisted with 0600/0700 permissions and atomic writes | `store::tests` (4) |
| Two daemons over real loopback QUIC with fake platforms | `two_daemons` |
| Proportional edge mapping between screens of different heights | `single_neighbour_maps_*`, `tall_mac_beside_short_pc_has_no_dead_zone`, `several_neighbours_*` |
| Escape recentres the cursor | `escape_and_peer_loss_both_end_driving_safely` |

## Hand test: a real Mac (3840×2160) driving a headless peer over loopback

| Check | Result |
|---|---|
| Pair two daemons: same 6-digit code on both, then pinned | ✅ |
| `kill -9` the peer: the other side stops swallowing, releases everything, disconnects | ✅ |
| Crossing the right edge at any height (after the proportional-mapping fix) | ✅ |
| While on the peer, keys and clicks don't reach Mac apps | ✅ |
| While on the peer, the Mac cursor is hidden | ✅ after 4 attempts, see below |
| Moving back left returns the Mac, with the cursor at the edge | ✅ |
| ⌃⌥⌘⎋ returns the Mac, with the cursor mid-screen | ✅ |
| `--exit-after` always gives input back | ✅ |

## Findings from the hand test

1. **Dead zone.** A tall Mac beside a short PC with tops aligned could only cross along
   the top half. Fixed with proportional edge mapping.
2. **The cursor kept moving while input was swallowed.** No public API stops this for a
   background process:
   - `CGAssociateMouseAndMouseCursorPosition` works only for the foreground app.
   - `CGWarpMouseCursorPosition` moves only WindowServer's position.
   - Posted hold events didn't hold the cursor either.

   Fixed by hiding the cursor through the private `SetsCursorInBackground` connection
   property, as Deskflow, Barrier and Synergy do. The user approved this.
3. **The escape chord looked like it did nothing.** The cursor reappeared at the edge and
   crossed straight back. It now reappears mid-screen.

## Known gaps

- A Mac with several displays is treated as its main display only.
- One session at a time (Free tier: 2 devices).
- Not run against a real second computer yet; that needs the Windows adapter.
