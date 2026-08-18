# ADR 0009: Paste-first injection

Date: 2026-08-05. Status: accepted.

## Problem

Typing a transcript into the focused window has two failure modes. First,
typing backends map control characters in the text to key presses: wtype
sends `\n` as Return, `\t` as Tab, `\e` as Escape (ydotool sends newlines
as Enter). A newline in a transcript therefore made the focused app submit
the text typed so far and then continue typing the rest into the result —
a "submitted halfway" corruption. Second, a long typing stream is fragile:
if the user switches focus mid-sequence, the remaining keys land in the
wrong window and the whole dictation must be redone. Constraining typed
output to a single flattened paragraph (no newlines) avoided the first
problem but threw away useful paragraph formatting, and did nothing about
the second.

## Decision

The default delivery becomes **paste-first**:

1. Write the finished transcript to the Wayland clipboard with `wl-copy`
   (it stays there afterward — `wl-clipboard` keeps holding the selection),
2. Send one `Ctrl+Shift+V` (`wtype -M ctrl -M shift -k v`, falling back to the
   `ydotool key 29:1 42:1 47:1 47:0 42:0 29:0` sequence) to paste it.

Nothing is typed into a live window until the whole text is already on
the clipboard, so paragraph breaks (blank lines) survive verbatim and a
focus change during dictation cannot interrupt delivery. The only
keypress is the single atomic paste.

`injection` gains a `paste` mode; `auto` prefers `[paste, wtype,
ydotool, clipboard]` and `type`/`clipboard` remain for the special
cases: typing never touches the clipboard and flattens newlines (they
cannot be typed safely), and `clipboard` only copies for the user to
paste by hand.

The original chord was `Ctrl+V`. TUIs such as OMP intercept that as
"read the host clipboard"; in a remote Herdr/SSH session that read is
empty even when `wl-copy` succeeded, and the app reports `Clipboard is
empty`. `Ctrl+Shift+V` is the terminal/compositor paste, so the Wayland
selection lands as bracketed paste. GUI apps that bind paste-as-plain-text
to the same chord still receive the transcript.

This supersedes the type-first chain in ADR 0002 (§Injection).

## Why paste instead of fixing typing

- Typing fundamentally cannot carry newlines safely: Return means
  "submit" or "enter" in normal apps and a literal control in TUIs, and
  there is no way for the sender to know which. Pasting delivers the
  exact text independent of the app's key bindings.
- An interrupted typing stream leaves partial text and requires a full
  retake; an interrupted paste cannot happen — the paste keypress is one
  event after the text is fully composed.

## Consequences

- Dictation results are now delivered via the clipboard: the previous
  clipboard contents are overwritten and not restored (same trade-off as
  the old clipboard fallback, already documented in README).
- The pill's success message reports `Pasted N chars (clipboard +
  Ctrl+Shift+V)`; typing reports `Typed N chars (wtype|ydotool)`.
- Users who must not touch the clipboard (sensitive fields) set
  `injection = "type"` and accept flattened paragraphs.
