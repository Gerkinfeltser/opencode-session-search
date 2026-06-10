# Opencode Session Search

Lists all the [Opencode](https://opencode.ai/) sessions across all folders and lets you fuzzy search. Has columns for title, last message, directory and date.

## Installation

### Cargo

```
cargo install --git https://github.com/kasbah/opencode-session-search
```

### Nix

```shell
nix run github:kasbah/opencode-session-search
```

Or add to your flake inputs:

```nix
inputs.opencode-session-search.url = "github:kasbah/opencode-session-search";
```

Then add it to your packages, e.g. in `home.packages` or `environment.systemPackages`:

```nix
inputs.opencode-session-search.packages.${system}.default
```

## Usage

```
opencode-session-search
```

- `<F2>` to switch between sorting by date or search score.
- Prefix searches with `title:`, `mes:` (last message) or `dir:` (directory) to restrict search to specific columns.
- Up/down arrows to select. Press enter to open the session in Opencode:
  - If the session belongs to the current folder's project it is resumed directly (`opencode -s <session_id>`).
  - Otherwise it is copied into the current folder via `opencode export` + `opencode import` (since opencode 1.15.12 sessions are pinned to the directory they were created in), and the copy is opened. The original session is untouched.
- Tested on Linux only for now.

![screenshot](https://raw.githubusercontent.com/kasbah/opencode-session-search/main/screenshot.png)
