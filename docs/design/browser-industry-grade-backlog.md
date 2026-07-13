# Browser — industry-grade gap backlog

Source: 6-lane multi-agent gap analysis vs Chrome/Edge/Firefox (2026-07-13, wf_ad423a0c-204;
73 raw gaps). The workflow's auto-synthesis returned a stub — this is the hand-synthesized
prioritized backlog recovered from the per-lane journal. Chrome = `crates/desktop/mde-shell-egui/src/web/`,
engine = `crates/desktop/mde-web-cef/`.

Engine (Track E) is now un-blocked: CEF paints real frames on the F44 seat after the
`/dev/shm` + `/etc/alternatives` sandbox fixes (`c27de915`, `c11c0ff4`).

## Tier 0 — blockers (a working browser needs these)
- **B1 CEF NavState → chrome.** CEF never emits nav/load state → omnibox URL, Back/Forward
  enable, and the loading indicator are all dead. *(engine lane; web-cef bridge + web/mod.rs)*
- **B2 Real downloads.** CEF download handler unwired — no browser-initiated downloads. *(downloads lane)*
- **B3 Browsing history absent.** History menu is only Back/Forward. *(downloads lane)*
- **B4 WebExtensions never run.** Validated + allowlisted but no runtime. *(extensions lane)*
- **B5 No password manager / autofill.** *(extensions + security lanes)*

## Tier 1 — high (industry-grade essentials)
- **Tab strip:** drag-reorder, keyboard shortcuts (Ctrl+T/W/Tab/Shift+T/1-9), restore-closed-tab,
  pinned tabs, audible/mute indicator, favicons. *(tabstrip lane; web/mod.rs, menubar.rs)*
- **Omnibox:** local history/bookmark autocomplete, inline autocomplete + top-hit preselect +
  keyboard nav of suggestions, clickable in-bar security indicator. *(omnibox lane)*
- **Engine input/paint:** page title from CEF, mouse button-state on move (drag-select/scrollbar),
  modifiers+click-count on click (ctrl/shift/double-click), `<select>`/datalist popups (PET_POPUP),
  in-page right-click context menu, window.open/target=_blank opens a visible tab. *(engine lane)*
- **Security:** cert-error interstitial, clickable page-info/site-info panel, per-site permission
  grants (camera/mic/location), safe-browsing blocked-page gating navigation. *(security lane)*
- **Bookmarks:** a bookmarks bar in the chrome; the star reflects state + is a toggle + add editor. *(downloads lane)*
- **Downloads UX:** Open / Show-in-folder on completed items; dangerous-file warnings. *(downloads lane)*

## Tier 2 — medium (polish toward parity)
- Omnibox: robust search-vs-navigate router, scheme elision + domain emphasis, configurable search
  engines + keywords/tab-to-search, IDN/punycode homograph handling.
- Tabs: favicons, shrink+scroll overflow (not stacked rows), middle-click close, tab groups,
  HTML5 page fullscreen, hover thumbnail.
- Engine: native page zoom (not CSS `zoom`), find-in-page match count + highlight-all, IME preedit,
  engine-driven cursor updates, JS dialogs/file chooser, sad-tab on renderer crash, favicons.
- Security/privacy: HSTS + persistent HTTPS-only, cookie viewer/controls, global clear-browsing-data
  dialog, adblock blocked-item breakdown + in-chrome filter toggle, explicit private-mode UX.
- Extensions/advanced: real userscript engine, inline page translate, readability reader mode,
  print preview + options (range/size/orientation), richer UA emulation, account sync, in-shell PDF UI.

## Deliberate non-goals (privacy threat model — confirm before "fixing")
- WebRTC removed → video conferencing/casting/screen-share impossible *(extensions lane flagged as
  blocker, but this is a deliberate design choice — re-confirm with operator before restoring)*.
- No persistent profile/cookies by design (sandbox has no writable $HOME) — private-by-default.

## Strengths (already above default Chrome)
- Every engine runs in a robust OS sandbox (userns + seccomp + dropped caps + pivot_root RO rootfs).
- Rich tab context menu (mute, force-dark, reader, containers, display target).
