# Source this file from ~/.zshrc after installing the lscat binary.
#
# Example:
#   source /path/to/lscat/shell/lscat.zsh

lscat() {
  local dir
  dir="$(command lscat --print-cwd "$PWD")" || return
  [ -d "$dir" ] && cd "$dir"
}
