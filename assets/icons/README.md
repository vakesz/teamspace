# Fluent UI System Icons (vendored)

Source: https://github.com/microsoft/fluentui-system-icons
Pinned commit: e15fc0de25fab983e3e42bac8798be69a85c23bf
License: MIT, see `LICENSE` in this directory (shipped with every release as
`licenses/Fluent-UI-System-Icons-LICENSE`).

Only the 24px `regular` variants we use are vendored, plus `filled` variants
for the rail icons that have a selected state. Files are named
`<icon>.svg` and `<icon>_filled.svg`. They are loaded by `src/ui/icon.rs`.

## Icons

Regular (34): `add`, `alert`, `apps`, `arrow_repeat_all`, `attach`,
`calendar_ltr`, `call`, `call_forward`, `call_inbound`, `call_missed`,
`call_outbound`, `chat`, `chevron_down`, `chevron_left`, `chevron_right`,
`chevron_up`, `cloud`, `compose`, `dismiss`, `drafts`, `edit`, `emoji`,
`filter`, `mention`, `mic`, `more_horizontal`, `panel_left`, `people_team`,
`person_add`, `search`, `send`, `sticker`, `text_font`, `video`.

Filled (6, the selected rail states): `alert_filled`, `calendar_ltr_filled`,
`call_filled`, `chat_filled`, `cloud_filled`, `people_team_filled`.

## Modification

Each file is the upstream `assets/<Name>/SVG/ic_fluent_<icon>_24_<variant>.svg`
with one modification: the upstream fill `#212121` is rewritten to `#FFFFFF`
so the rasterized texture is white and `egui::Image::tint` yields the exact
tint color:

    sed -i.bak 's/#212121/#FFFFFF/g' assets/icons/*.svg && rm assets/icons/*.bak

A unit test in `src/ui/icon.rs` rejects any vendored SVG that still contains
`#212121`.

## Other assets and licences

- App icon: `assets/app-icon/teamspace.svg` is the source (1024 px, part of
  this project and covered by the repository `LICENSE`); `teamspace-1024.png` is its
  master render and `teamspace-256.png` is the window icon embedded by the app.
  `scripts/bundle-macos.sh` renders the macOS `.icns` from the 1024 px master.
- Fonts: Inter (Regular, SemiBold) in `assets/fonts/`, SIL Open Font License 1.1,
  see `assets/fonts/OFL.txt`.
- `assets/layout-mockup.png` and `assets/supply-costs.png` are
  local-preview attachment images.
