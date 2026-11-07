# `odignored` - OneDrive Ignore Daemon

Give OneDrive `.gitignore` like powers!

Daemon that will identify when you have files that are marked as ignorable, and add them to OneDrive's exclusion list

## Ignore Types

- `.gitignore` (powered by the `ignore` crate)
- `.odignore`


## Usage

```powershell
odignored scan                      # dry run: show what would be blocked (no admin needed)
odignored daemon --cooldown 30s     # background process with a system tray icon (prompts for UAC)
odignored run                       # same, but attached to the console
odignored stop
```

- Watches every OneDrive account registered with the sync client. Use `-x <folder>` to skip one
  (e.g. an old account's folder) or `--root <folder>` to watch only specific folders.
- Right-click the tray icon for **Open log** / **Quit**. Quitting relaunches OneDrive if we had stopped it.
- The log lives in `%LOCALAPPDATA%\odignored\odignored.log`.
- `--no-tray` runs headless.

`.onedriveignore` uses gitignore syntax. All `.onedriveignore` files beat all `.gitignore` files,
so `!keep.log` there re-includes something git ignores.

## How it works

Ignored entries are written to OneDrive's admin policy lists
(`HKLM\SOFTWARE\Policies\Microsoft\OneDrive\EnableODIgnoreListFromGPO` / `...EnableODIgnoreFolderListFromGPO`).
The daemon needs admin for that, so it asks for UAC once at startup.

- **New ignorable entry** -> policy updated, OneDrive killed immediately, relaunched after a quiet
  `--cooldown` (capped at 5x so constant churn can't keep it down). Only relaunched if we stopped it.
- **Ignore file edited** -> full rescan.
- **Ignored entry deleted** -> its policy entry is pruned *without* restarting OneDrive.
- OneDrive is relaunched de-elevated (it refuses to run as admin).

## Limits of the OneDrive policy (not of this tool)

- Matches by **name only**, not path: ignoring `/build/out.txt` would block every `out.txt`.
  To avoid collateral damage, a name is **not blocked if any non-ignored entry with that name exists**
  in the watched folders (`scan` lists these as `skip`).
- Only affects **new** files; anything already synced stays synced.
- Folder names have no wildcards.
- The daemon owns both policy lists and overwrites other entries in them.
