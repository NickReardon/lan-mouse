# First iteration: bidirectional Windows–macOS features

Status: scope and acceptance plan, not a statement that every feature is implemented or tested.
Updated: October 10, 2026.

## Goal

Use either computer's physical mouse and keyboard to control the other computer
across a local network, with predictable screen transitions, native keyboard
shortcuts, and text clipboard sharing. Make connection setup and device placement
understandable without requiring users to maintain conflicting edge settings.

Windows and macOS are the immediate implementation and manual-validation focus.
Linux is part of the product and feature scope; Linux testing is deferred and
does not block this iteration's Windows–macOS acceptance. Deferred testing must
remain visible rather than being reported as a pass or as removal of Linux support.

The platform priority and native shortcut behavior are confirmed requirements.
The feature grouping below proposes the first-iteration boundary for review;
it does not approve additional implementation work or mark existing issues complete.

## Platforms and directions

| Pair | Required behavior | Validation timing |
|---|---|---|
| Windows → macOS | Windows physical input controls the Mac; return control works | First iteration |
| macOS → Windows | Mac physical input controls Windows; return control works | First iteration |
| Windows ↔ Linux | Same applicable feature requirements, subject to Linux backend capabilities | Deferred |
| macOS ↔ Linux | Same applicable feature requirements, subject to Linux backend capabilities | Deferred |

Bidirectional means either device can initiate control after reciprocal setup.
It does not mean both devices inject input into each other simultaneously.
An active client receives events; an inactive client can send events back.
Keep this mutual exclusion to prevent feedback loops.

Start manual testing with the Windows 11 desktop and M2 MacBook recorded in
[issue #8](https://github.com/NickReardon/lan-mouse/issues/8). Record the actual
OS versions, build revision, monitor layout, scaling, permissions, and network
configuration with the results. The Windows/Ubuntu dual-boot laptop remains a
later test target. Confirm its Ubuntu version and desktop/session before testing.

## Proposed first-iteration feature boundary

| Feature | First-iteration requirement | Tracking and current evidence |
|---|---|---|
| Bidirectional mouse and keyboard | Either device can initiate control; movement, buttons, dragging within applications, scrolling, typing, and modifiers work; return by edge or release bind | [#8](https://github.com/NickReardon/lan-mouse/issues/8); existing architecture, manual validation required |
| Relative cursor handoff | Preserve relative height on left/right crossings and relative width on top/bottom crossings, including return crossings | [#17](https://github.com/NickReardon/lan-mouse/issues/17); local implementation documented in DOC.md, manual validation pending |
| Native shortcuts | Use Ctrl-based shortcuts on Windows/Linux and Command-based shortcuts on macOS; the user chooses the destination OS's shortcut | [#7](https://github.com/NickReardon/lan-mouse/issues/7); native behavior required, configurable remapping remains a later extension |
| Text clipboard | Opt-in Unicode plain-text transfer on control switches in both directions; fetch remote text when returning control | [#1](https://github.com/NickReardon/lan-mouse/issues/1); Windows/macOS local implementation, manual validation pending; Linux adapter remains future work |
| Visual device layout | Compact arrangement in the main window, reciprocal edges, synchronization, and persistence | [#6](https://github.com/NickReardon/lan-mouse/issues/6), detailed design in [#16](https://github.com/NickReardon/lan-mouse/issues/16); planned feature, not established by a mockup |
| Manual connection and authorization | Hostname/IP setup, authorization in both directions, useful failure messages, and retained configuration | [#8](https://github.com/NickReardon/lan-mouse/issues/8), [#9](https://github.com/NickReardon/lan-mouse/issues/9); validate existing setup and track authentication gap separately |
| Recovery | Release held input on disconnect, unexpected peer exit, or termination; recover local control without restarting the OS | [#8](https://github.com/NickReardon/lan-mouse/issues/8); validate before acceptance |
| Runnable packages | Windows ZIP and macOS app with required runtime dependencies; document permissions and launch steps | [#11](https://github.com/NickReardon/lan-mouse/issues/11); package availability alone does not establish successful launch |

### Input and keyboard behavior

Test ordinary desktop applications in both directions, including left/right
click, click-and-drag, vertical and horizontal scrolling, text entry, and held
modifier combinations. Record unsupported keys or OS-reserved combinations as
limitations rather than promising identical interception on every platform.

Native shortcuts are intentional: when controlling a Mac, use Command+C/V;
when controlling Windows or Linux, use Ctrl+C/V. This iteration does not require
automatic Ctrl↔Command remapping. Validate which physical key produces Command
on a Windows keyboard and Ctrl on a Mac keyboard and document the result.
Configurable modifier mapping and direct-device-switch hotkeys remain in #7;
the existing emergency release bind stays available.

### Screen transitions and layout

Treat each device tile as its entire desktop. Preserve edge fractions across
different resolutions, display scaling, and negative desktop origins. Enter
just inside the destination edge to avoid an immediate bounce. For desktop
gaps, use the nearest real segment of the destination's outer edge as described
in [DOC.md](DOC.md#relative-edge-position-windows-and-macos).

Use the layout design in #16: one primary device is the docking anchor, with up
to four remote devices on its left, right, top, and bottom. Allow editing from
either authorized device, dragging and snapping, occupied-edge swaps, and a
keyboard-accessible equivalent. Keep connection labels, reciprocal mappings,
and persisted settings consistent. Resolve concurrent edits deterministically
and reconcile offline edits without synchronization loops.

The primary designation is a layout anchor, not exclusive permission to send
input. Each connected pair must still support control in both directions.
Remote devices do not gain indirect routing through the primary. Arbitrary
chains and exact per-monitor alignment are later work.

### Text clipboard

Use the documented `on-switch` mode, disabled by default. Transfer plain text
up to 1 MiB of UTF-8 over the independently authenticated TLS clipboard channel.
Pin the destination certificate and authorize the controller. Clipboard access
must work with the settings window closed and must not block input processing.

Test Unicode, emoji, multiline text, rapid switches, local copy during a
transfer, timeout, and disconnection. Unsupported clipboard contents must leave
the destination unchanged. Do not log clipboard contents. Synchronization is
asynchronous: immediate paste may still see the previous clipboard.

See [README.md](README.md#clipboard-sharing-windows-and-macos) for configuration
and [DOC.md](DOC.md#clipboard-protocol-v1) for limits, protocol, and manual checks.
The current native adapters cover Windows/macOS only. Linux text clipboard
support remains in scope under #1; it is not currently validated or delivered
by those adapters.

## Linux scope and deferred validation

Keep shared input events, protocol behavior, configuration, layout state, and
clipboard interfaces suitable for Windows, macOS, and Linux. Preserve existing
Linux paths and document any new platform-specific limitation. Shared logic
should not assume Windows desktop coordinates or macOS APIs.

Linux is not a single capture/emulation target. Before Linux implementation or
manual testing, select the desktop/session and confirm the applicable backend:
libei, layer-shell, wlroots, remote-desktop portal, or X11. The current README
documents X11 capture as unsupported/WIP; do not promise bidirectional X11
control as part of an untested Linux claim. Proportional edge handling currently
leaves Linux backend behavior unchanged and needs separate backend work.

Deferred Linux acceptance includes Windows↔Linux and macOS↔Linux control,
clipboard, native shortcuts, layout synchronization, recovery, permissions,
and compositor-specific restrictions. Record each result by backend/session.
Until these checks run, label Linux feature parity **in scope, unverified**.

## Later feature extensions

These remain in the product scope but are proposed outside first-iteration acceptance:

| Extension | Tracking |
|---|---|
| Image and rich-text clipboard | [#2](https://github.com/NickReardon/lan-mouse/issues/2) |
| File/folder copy and paste with progress, cancellation, and conflict handling | [#3](https://github.com/NickReardon/lan-mouse/issues/3) |
| File/folder drag and drop across computers | [#4](https://github.com/NickReardon/lan-mouse/issues/4) |
| Automatic peer discovery, retaining manual setup and explicit authorization | [#5](https://github.com/NickReardon/lan-mouse/issues/5) |
| Optional modifier remapping and direct-device-switch hotkeys | [#7](https://github.com/NickReardon/lan-mouse/issues/7) |
| Continuous clipboard synchronization and remote-paste mode | [#1](https://github.com/NickReardon/lan-mouse/issues/1) records the initial on-switch boundary |
| Arbitrary device chains, indirect forwarding, precise monitor alignment | [#16](https://github.com/NickReardon/lan-mouse/issues/16) records the layout boundary |

Outgoing input peer authentication is tracked in [#9](https://github.com/NickReardon/lan-mouse/issues/9).
Do not describe input encryption as proof of mutual peer authentication.
The clipboard channel must satisfy its own identity checks regardless of the
input-channel work. Future file transfer depends on authenticated reliable transport.

## Windows–macOS acceptance checklist

Run every applicable scenario with Windows controlling macOS and macOS
controlling Windows. A return crossing alone does not prove the other device
can initiate control with its own physical keyboard and mouse.

- [ ] Launch both packaged applications, grant required permissions, configure
  reciprocal clients/edges, and authorize both directions.
- [ ] Initiate control from either physical keyboard/mouse; exercise movement,
  clicks, dragging within applications, scrolling, typing, and modifiers.
- [ ] Cross all four edges at 10%, 50%, and 90%; verify proportional entry and
  return after moving along the remote edge. Repeat with different resolutions,
  scaling, negative monitor origins, and desktop gaps.
- [ ] Use native copy/paste and navigation shortcuts on the destination;
  record physical modifier-key behavior and OS-reserved limitations.
- [ ] Complete the clipboard manual checks in DOC.md in both directions,
  including authorization failures and unsupported content.
- [ ] Edit layout from either side; test snapping, swaps, primary selection,
  keyboard access, restart persistence, offline edits, and concurrent edits.
  Validate four occupied primary edges separately when enough devices are available.
- [ ] Return by edge and emergency release; disconnect or exit a peer while
  modifiers and mouse buttons are held; confirm local control and released input.
- [ ] Reconnect, restart, and reload configuration without feedback loops,
  obsolete clipboard writes, or lost layout settings.
- [ ] Record build revisions, actual checks, failures, and untested scenarios.
  Create separate bug issues only for reproduced failures; label other concerns
  as investigations. Linux scenarios remain explicitly deferred.

For implementation changes, run affected crate tests, formatting, and workspace
Clippy as required by AGENTS.md. Automated checks supplement native manual
verification; they do not establish cross-platform acceptance by themselves.

First-iteration acceptance requires evidence for the agreed Windows–macOS
features. Any missing feature or unavailable check needs an explicit recorded
scope adjustment or exception. Publishing a release is a separate action.
