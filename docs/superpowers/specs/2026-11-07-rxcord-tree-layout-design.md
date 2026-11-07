# rxcord — TeamSpeak-style compact layout

Date: 2026-11-07
Status: Approved in chat (user waived the written-spec review: "do it all without askin for consent")

## Goal

Replace the Discord-like three-column window with a small, single-window layout in the style of
TeamSpeak: one tree on top, chat tabs below, a status bar at the bottom. The window should sit next to a
game without taking over the screen. Data, networking and the store are unchanged.

## Layout

```
┌──────────────────────────────┐
│ ▾ Direct Messages            │
│     Bob                    2 │  mention badge
│ ▸ Gaming Server            ● │  collapsed: aggregated unread dot / badge
│ ▾ Rust Community             │
│   GENERAL                    │  category (not collapsible)
│       # help                 │  bold = unread
│       ♪ Lounge               │  voice: greyed (stage 2 lists people here)
├──────────────────────────────┤  draggable divider
│ [Bob ×] [#help ×]            │  tabs
│ 12:01 Bob: are you on?       │  compact rows, no avatars
│ ┌ type a message… ─────────┐ │
├──────────────────────────────┤
│ ● Connected · name         ⚙ │  status bar; ⚙ → Log out
└──────────────────────────────┘
```

- Window: preferred 360×640, minimum 280×400. Size, position (not on Wayland) and divider position are
  restored from `settings.json`.
- Tree: DMs node first, then every guild. Click a node row to expand/collapse. Collapsed nodes show the
  aggregated unread dot and mention badge (same rules as the old guild list: voice, categories and
  403-forbidden channels don't count). All nodes start collapsed except the last-used guild (persisted)
  and DMs.
- Tabs: clicking a text channel or DM opens it as a tab, or activates its existing tab. Close with × or
  middle click. At most 8 tabs; opening a 9th closes the least recently used one. Tabs show bold when
  unread and a mention badge. Background tabs stay live (their messages keep updating).
- Chat rows: `HH:MM Name: text` on one line, wrapping; reply preview line above; thumbnails at most
  200 px; failed rows keep the Retry action inline.
- Message actions move to a right-click menu (Reply, Edit, Delete, Copy text, Open link of the first
  attachment). Delete still asks for a second confirmation (menu item "Delete…" then a confirm item).
- Status bar: connection state (empty banner = "Connected") and your display name; ⚙ opens a popup
  with Log out.
- Original look: rxcord's own palette and glyphs, no TeamSpeak artwork.

### Changes made during the build (user requests and findings)

- Messages are spaced cards with a background (user request); failed ones have a red tint.
- Thin borders between tree, tabs, messages, composer and status bar; the divider is a visible line.
- Composer is one line (Shift+Enter still adds lines, which scroll inside).
- Each message is one `StyledText` (header + paragraphs + code as inline-code lines): repeated text
  elements in a list row were measured unwrapped, so wrapped messages overlapped the next row.
  Code blocks lose their framed box. Links inside messages are now clickable.
- No "Copy text" menu item: Slint has no clipboard API and it would need a new dependency.

## Behaviour rules

- The active tab is the old "selected channel": composer, drafts, acks, history paging and
  notifications keep their current rules.
- Closing a tab drops that channel's loaded messages from the store (memory), keeps its draft.
- A tab whose channel is deleted (or whose guild is left) closes. If it was the active one, the composer
  text moves into the next active tab's draft with the notice "Channel was deleted — unsent text moved
  here" (fixes stage 1 follow-up #1 for the active channel).

## Code changes

- `nav.rs`: `Tabs` (open/activate/close/retain, LRU eviction at 8) and the expanded-node set replace
  `guild`; `selected` stays = active tab.
- `view.rs`: `tree_rows(store, expanded, forbidden) -> Vec<TreeRow>` and `tab_rows`.
- `store`: `forget_channel(id)` drops loaded messages.
- `settings.rs`: `settings.json` in the config dir (`$XDG_CONFIG_HOME` or `~/.config` / `%APPDATA%`),
  `rxcord/settings.json`; window size/pos, divider, last guild. Never token or message text.
- UI: `tree.slint`, `tabs.slint`, compact `message_row.slint` with `ContextMenuArea`, new `app.slint`
  layout and status bar. `sidebar.slint` removed.
- Demo mode shows the new layout.

## Testing

Unit tests (test-first): tabs (open, reopen, LRU eviction, close-active picks a neighbour, retain on
delete), tree rows (order, depth, collapsed aggregation, DMs first, expanded children), settings
round-trip and corrupt-file fallback, `Drafts::carry`. Manual: checklist in `docs/stage1-checks.md`;
idle CPU re-measured with `scripts/idle-check`.
