# Manga Reader

Full plan and technology choices: @PLAN.md

## Language

- **Everything in this repository is English**: code, identifiers, comments, UI strings, docs, commit messages, branch names, PR/issue text. No Turkish anywhere on GitHub.
- Chat with the user in Turkish (conversation only, never committed).

## Context

- The app runs on this machine: ThinkPad X1 Yoga Gen 4 (Intel UHD 620, 16 GB RAM), Fedora KDE Plasma (Wayland).
- Usage: folded 360° into **tablet mode**, **touch + pen**, fan mostly off. Reading style: **vertical scrolling** (webtoon).
- Priority: low heat and smooth scrolling > number of features. Judge every decision by its CPU/GPU cost.

## Rules

- Rust + GTK4 (`gtk4-rs`) + our own retro CSS theme (**no** libadwaita). Ask the user before switching to another UI toolkit.
- Visual style: late-90s "platinum" retro desktop (PLAN.md → Visual style). No copied logos/icons; icons are original.
- The reader is true full screen; the control panel opens only via triple tap / triple click / pen side button / Esc. Single and double taps do nothing.
- Downscale images to screen width, split long strips into tiles, release off-screen tiles, run heavy work on low-priority threads.
- No GPU-heavy effects: no blur, shadows or continuous animation.
- Smoothness is non-negotiable: never decode, touch disk/network or run JS on the main thread; the reader is a custom `StripView`, not `GtkListView` (PLAN.md → Smoothness architecture). Pinch zoom is a GPU transform only.
- GTK treats the pen like a mouse: in the reader, dragging with the pen must scroll.
- Komikku/Mihon are feature references only; the UI and name are original.
- Never ask for or store the user's sudo password; when root is needed, tell the user the command to run themselves.

## Current phase

Phases 0, 1, 2 (downloads only) and 4 (anime) are implemented; SiteB is blocked and Phase 3 (Suwayomi) is skipped on purpose: see PLAN.md → *Scope decisions*.
