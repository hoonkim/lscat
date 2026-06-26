# lscat

`lscat` is a Rust terminal file explorer designed for kitty and other modern terminals.

It opens at the current working directory by default. When used through a shell
function, pressing `Esc` or `q` closes the explorer and changes the parent shell
to the directory currently shown in the UI.

## Install

```sh
cargo install --path .
```

Or install from Homebrew:

```sh
brew install hoonkim/tap/lscat
```

## Release

The release workflow runs when a `v*` tag is pushed:

```sh
git tag v0.1.0
git push origin v0.1.0
```

It publishes a source archive, then updates `hoonkim/homebrew-tap` with a
formula that builds lscat with Cargo during `brew install`. The repository must
have a `HOMEBREW_TAP_TOKEN` secret with write access to that tap.

## Shell integration

Add this to `~/.zshrc` if you want `lscat` itself to change your shell's
current directory when it exits:

```sh
lscat() {
  local dir
  dir="$(command lscat --print-cwd "$PWD")" || return
  [ -d "$dir" ] && cd "$dir"
}
```

Or source the bundled zsh integration:

```sh
source /Users/kimh/lscat/shell/lscat.zsh
```

Then run:

```sh
lscat
```

## Spawn a shell

If you do not want to wrap your current shell, run:

```sh
lscat --spawn-shell
```

When the explorer exits, it starts a new shell in the directory currently shown
in the UI. Exiting that shell returns to the original shell.

While lscat is running, press `!`, type a command, then press `Enter`.
lscat temporarily leaves the TUI, runs that command in the current lscat
directory, then waits for `Enter` before returning so command output remains
visible.

## File openers

Create `.lscat/config.yaml` in the directory where you run `lscat`, or create
`~/.lscat/config.yaml` for a global config:

```yaml
openers:
  md:
    command: "mdcat {file}"
    mode: "kitty-window"
  rs: "vi {file}"
  txt:
    command: "cat {file}"
    wait: true
  png:
    command: "kitten icat {path}"
    mode: "kitty-overlay"
    wait: true
```

When a matching file is opened with `Enter` or double click, lscat temporarily
leaves the TUI, runs the configured command, and returns after the command exits.
Set `wait: true` when you want lscat to wait for `Enter` before returning, which
is useful for commands like `ls` or `mdcat` where the output matters.

Set `mode` when you want command output to stay out of the lscat terminal
scrollback:

```text
inline         Run in the current terminal
kitty-overlay  Run in a kitty overlay
kitty-tab      Run in a new kitty tab
kitty-window   Run in a new kitty window
```

Available placeholders:

```text
{file}  File name relative to the current lscat directory
{path}  Full file path
{name}  File stem without extension
```

If a command has no placeholder, lscat appends `{file}` automatically.

## Keys

```text
Path click     Jump to that directory
Click          Select item
Double click   Open selected directory
Right click    Go to parent directory
Wheel          Scroll file list
Up/Down, k/j   Move selection
/              Search in current directory
Up/Down in /   Move within search results
!COMMAND       Run command here, then return to lscat
Enter, l       Open selected directory
Left, h        Go to parent directory
g/G            Jump to first/last item
r              Refresh directory
Esc, q         Close and return the current directory
```
