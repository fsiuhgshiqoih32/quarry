# Changelog

## 0.1.1

- The window now opens on machines without OpenGL 2 (many virtual machines, remote desktops and
  PCs without GPU drivers): Quarry falls back to wgpu, using Direct3D 12 with a software adapter
  on Windows when there's no GPU. `QUARRY_RENDERER=wgpu` skips the OpenGL attempt.
- Linux: the `.deb` now depends on `libxkbcommon-x11-0`, which the window needs.

## 0.1.0

First release.

- Instant name search over every local drive (Windows) or the whole file system (macOS, Linux),
  with Everything's syntax: AND, OR (`|`), NOT (`!`), `< >` groups, quotes, wildcards, path terms,
  and `ext:`, `size:`, `dm:`, `file:`, `folder:`, `parent:`, `case:`, `ww:`, `regex:`, `path:`,
  `startwith:` and `endwith:`.
- Live updates from ReadDirectoryChangesW, FSEvents and inotify, a saved index for instant start,
  and optional periodic rescans.
- Results table sorted by name, path, size or date, with matches highlighted; keyboard navigation,
  multi-select, context menu, and dragging files out to other apps (Windows and macOS).
- Tray icon, global shortcut (Alt+Shift+Space), start at login, single window per user, and
  `quarry --query` to open the window with a search.
- `quarry search`, `index`, `serve` and `paths` commands, JSON output, and `quarry-cli.exe` for
  Windows scripts.
- Read-only HTTP API on 127.0.0.1 with Host-header checks.
- Settings for folders, skipped folders, hidden files, match path, theme and the API.
