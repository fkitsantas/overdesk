## What this changes

<!-- One or two sentences: what the user (or a developer) gets, and why. Link the issue it closes: "Closes #123". -->

## How it works

<!-- The approach in a few lines, and anything a reviewer should look at first. Leave out what the diff already shows. -->

## Type

- [ ] Bug fix
- [ ] New feature
- [ ] Performance
- [ ] Refactor (no behaviour change)
- [ ] Documentation
- [ ] Build, packaging or CI

## Tested on

<!-- Input capture and injection differ per OS: say where you ran it, with the OS version and, on Linux and the BSDs, the desktop and X11 or Wayland. -->

- [ ] macOS:
- [ ] Windows:
- [ ] Linux:
- [ ] FreeBSD / OpenBSD:
- [ ] Hand tests from [docs/HAND-TESTS.md](https://github.com/fkitsantas/overdesk/blob/main/docs/HAND-TESTS.md) that this touches:

## Checklist

- [ ] `./scripts/check.sh` passes (fmt, clippy with `-D warnings`, tests, docs with `-D warnings`, licences). Windows: `scripts\check.ps1`.
- [ ] If platform code changed: `./scripts/check-cross.sh` passes (clippy for Windows, Linux and FreeBSD).
- [ ] New behaviour has a test, or the PR says why it can't have one.
- [ ] Every new public and private item has a doc comment (the `missing_docs` lints are on).
- [ ] No `unsafe` outside the platform FFI modules, and each `unsafe` block says why it is sound.
- [ ] No new dependency, or the PR says why the standard library and the existing ones aren't enough (licence must pass `cargo deny`).
- [ ] Privacy holds: nothing logs or sends keystrokes, clipboard contents, PINs, keys, tokens or email addresses.
- [ ] README.md and the docs are updated where behaviour or setup changed.
- [ ] I have signed the [CLA](https://github.com/fkitsantas/overdesk/blob/main/CLA.md) (the CLA bot asks on your first pull request).

## Screenshots or recordings

<!-- For anything visible in the app window, the tray menu or the Desk map: before and after, light and dark. Delete this section otherwise. -->
