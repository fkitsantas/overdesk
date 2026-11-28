# TDD evidence: foundation (phases 0–1)

> **Historical record.** This is the evidence of the first two phases as they were on 2026-11-26.
> It is not kept up to date: test names and commits may since have moved or been renamed, and
> the code has grown far past it. For the present state read the tests and
> [ARCHITECTURE.md](../ARCHITECTURE.md).

Source plan: the `/plan` conversation of 2026-11-26. The journeys were written during this run.

## User journeys

- As a Mac user with a Windows PC, I want the cursor to cross a screen edge onto the other computer, so that one mouse drives both.
- As that user, I want ⌘+C on my Apple keyboard to copy on Windows, so that my shortcuts keep working.
- As any user, I want no stuck keys when focus moves or a peer drops.
- As a peer, I want malformed or oversized frames rejected, so that a hostile peer can't crash me or exhaust my memory.

## Cycles

| # | Behaviour | RED commit | GREEN commit |
|---|---|---|---|
| 1 | Proto framing and core switcher / held keys | `02bd4b2`: 17/17 failed at `todo!()` | `d0d202d`: 17/17 pass |
| 2 | ⌘↔Ctrl remap | `test: add failing Mac<->PC…`: 3 failed at `todo!()` | `4d84984`: 20/20 pass |
| 3 | Review fixes: `encode` size limit, bounds on layout width, unknown start screen | `test: add regression tests…`: compile-time RED | `fix: …`: 22/22 pass |

## Guarantees

| What is guaranteed | Test |
|---|---|
| Every message round-trips through framing | `proto::every_message_round_trips` |
| Partial frames ask for more bytes and never error | `proto::partial_frame_needs_more_bytes` |
| A length prefix over 1 MiB is rejected before allocating | `proto::oversized_length_rejected_without_payload` |
| A message over 1 MiB refuses to encode | `proto::oversized_message_refuses_to_encode` |
| Garbage or trailing bytes are `Malformed` and don't panic | `proto::garbage_payload_is_malformed`, `trailing_bytes_inside_frame_are_malformed` |
| Layouts that are empty, zero-size, duplicate, overlapping or overflow `i32` are rejected | `core::layout_rejects_invalid` |
| Crossing an edge switches screens with the correct local entry point | `core::crossing_right_edge_switches_with_local_entry_point` |
| A fast flick lands on the neighbour instead of clamping | `core::fast_flick_past_neighbour_lands_on_its_far_edge` |
| An edge with no neighbour clamps | `core::edge_without_neighbour_clamps`, `taller_neighbour_region_without_counterpart_clamps` |
| A diagonal exit through a corner switches | `core::diagonal_corner_exit_still_switches` |
| Held keys are released once, in order | `core::held_keys_released_once_in_order` |
| ⌘↔Ctrl swap is applied only between Mac and PC | `core::mac_to_pc_*`, `pc_to_mac_*`, `same_family_and_other_keys_pass_through` |

## Commands

```
cargo test --workspace                                  # 22 passed
cargo clippy --workspace --all-targets -- -D warnings  # clean
cargo fmt --all --check                                 # clean
cargo deny check                                        # advisories, bans, licenses, sources ok
```

## Known gaps

- No coverage figure: `cargo-llvm-cov` isn't installed. Every public function has tests.
- `decode` hasn't been fuzzed.
- No platform code exists yet, so nothing touches a real cursor.
