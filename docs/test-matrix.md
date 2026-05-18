# WebTerminal Regression Test Matrix

This matrix exists to prevent regressions around tmux -CC resize/fix, mobile single-pane display, replay, and visible flashing.

## Protocol Invariants

| Case | Setup | Action | Required result |
| --- | --- | --- | --- |
| Initial attach before tmux panes | tmux-control session starting/attaching | Open websocket | Startup/password/direct output remains replayable; no blank screen while panes are not known. |
| Multi-client sync | Two websocket clients on same session | Rename tab, note pane, split, reconnect | Names and notes survive reconnect and pane deletion cleans stale notes. |
| Plain resize | tmux-control single pane | POST `/resize` without `zoom_pane_id` | Mode stays tmux-control; names/notes survive; no command fallback to raw tmux. |
| Mobile fix on split window | tmux-control window with split panes | POST `/resize` with focused `zoom_pane_id` | Target pane is zoomed to requested cols/rows; window reports `zoomed=true`. |
| Fix must not flash | Active websocket focused on pane | POST `/resize` with or without zoom | No `clear` server message is broadcast by resize/capture; no large replay burst is sent to active client. |
| Replay cache only | Backend runs `capture-pane` after state refresh | Active websocket remains connected | Capture updates backend replay cache only; active viewport is not clear/rewrite refreshed. |

## Browser/UI Matrix

| Viewport | tmux layout | UI state | Action | Metrics to check |
| --- | --- | --- | --- | --- |
| 390-600px wide | 3 panes split | tools closed | Fix size | `.desktopPaneGrid` hidden, visible `.terminalSurface` measured; focused pane zoomed; screen gap <= 16px. |
| 390-600px wide | 3 panes split | tools open | Fix size | Terminal content remains nonblank for 3s; no visible flicker; keybar does not overlap surface. |
| 390-600px wide | zoomed pane | long input | Type 100+ chars | Text wraps at near visible right edge, not half-width; no missing second line. |
| 390-600px wide | zoomed pane | refresh | Browser reload | Same focused pane and replayed content return; no blank page. |
| 1280px wide | 3 panes split | desktop grid visible | Fix size | Uses desktop grid measurement; all panes have `leakRight=0` and `leakBottom=0`. |
| Any | one pane producing output | another pane scrolled up | Output continues | Scrolled pane viewport is not forced to bottom. |
| Mobile drawer | open/close session drawer and session menu | terminal active | Toggle drawer | Drawer never covers terminal after close; borders are intact; no text occlusion. |

## Manual Debug Signals

- Backend log must not show repeated `websocket output receiver lagged` while idle.
- After mobile fix on split layout, `tmux list-windows -F '#{window_zoomed_flag}'` should be `1`.
- Focused pane width from `tmux list-panes -F '#{pane_width}x#{pane_height}'` should match the session cols/rows after fix.
- Browser DOM checks:
  - visible nonblank terminal lines should not drop to zero after fix.
  - `.terminalSurface.right - .xterm-screen.right <= 16` on narrow single-pane display.
  - no repeated `clear` messages should be observed during resize/fix.
