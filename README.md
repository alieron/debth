# debth
TUI code review app to help keep track of your vibe coded project's technical debt.

## Run

```sh
cargo run
```

On first start in a git repo, `debth` creates `.debth/` for review state and asks whether it should add `.debth/` to `.gitignore`.

## Keys

- `Enter`: open a file or expand/collapse a directory
- `1`/`2`/`3`: focus review overview, file explorer, or file viewer
- `Tab`: cycle between panes
- `Space`: expand/collapse the selected directory
- `Up`/`Down` or `k`/`j`: move selection or current review line
- `Left`/`Right`: collapse or expand directories in the explorer
- `a`: accept the selected file in the explorer or current line in the viewer
- `r`: reject the selected file in the explorer or current line in the viewer
- `u`: mark the current line unreviewed
- `q`: quit
