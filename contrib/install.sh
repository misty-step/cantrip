#!/usr/bin/env bash
# Binary-only maintenance: startup owners and all operator data stay untouched.
set -euo pipefail
umask 077

fail() {
  printf 'cantrip installer: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage: ./install.sh install   [--prefix PATH]
       ./install.sh update    [--prefix PATH] --backup PATH
       ./install.sh rollback  [--prefix PATH] --backup PATH
       ./install.sh uninstall [--prefix PATH]

The default prefix is $HOME/.local. Only PREFIX/bin/cantrip is installed.
Update creates a new backup file; rollback reads it without consuming it.
Stop the existing daemon through its owner first. No services, shortcuts,
configuration, models, recordings, or keyring entries are changed.
USAGE
}

[[ $# -gt 0 ]] || { usage >&2; exit 2; }
operation=$1
shift
case "$operation" in
  -h|--help) usage; exit 0 ;;
  install|update|rollback|uninstall) ;;
  *) fail "Unknown operation '$operation'; use --help." ;;
esac
prefix=
backup=
prefix_seen=false
backup_seen=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix|--backup)
      [[ $# -ge 2 && -n "$2" ]] || fail "$1 requires a path."
      if [[ "$1" == --prefix ]]; then
        [[ "$prefix_seen" == false ]] || fail '--prefix was supplied twice.'
        prefix=$2
        prefix_seen=true
      else
        [[ "$backup_seen" == false ]] || fail '--backup was supplied twice.'
        backup=$2
        backup_seen=true
      fi
      shift 2
      ;;
    *) fail "Unexpected argument '$1'; use --help." ;;
  esac
done
if [[ "$prefix_seen" == false ]]; then
  [[ -n "${HOME:-}" ]] || fail 'HOME is unset; supply --prefix PATH as the intended Cantrip user.'
  prefix=$HOME/.local
fi
case "$operation" in
  update|rollback) [[ "$backup_seen" == true ]] || fail "$operation requires an explicit --backup PATH." ;;
  *) [[ "$backup_seen" == false ]] || fail "--backup is only valid for update and rollback." ;;
esac
for command in cat stat sha256sum readlink dirname mkdir rmdir mktemp install ln mv rm; do
  command -v "$command" >/dev/null || fail "Missing '$command'; install GNU coreutils and retry."
done

# Resolve syntax without following links, including links in path ancestors.
# Reject '..' instead of silently normalizing away a symlink traversal.
absolute_path() {
  local value=$1 part current=
  local -a parts
  [[ -n "$value" && "$value" != *$'\n'* && "$value" != *$'\r'* ]] || fail 'Paths must be nonempty and contain no newlines.'
  [[ "$value" == /* ]] || value="$(pwd -P)/$value"
  IFS=/ read -r -a parts <<< "$value"
  for part in "${parts[@]}"; do
    [[ -n "$part" && "$part" != . ]] || continue
    [[ "$part" != .. ]] || fail "Use a path without '..': $value"
    current+=/$part
    [[ ! -L "$current" ]] || fail "Refusing symlink path: $current"
  done
  REPLY=${current:-/}
}

# Root-owned sticky directories such as /tmp are safe ancestors, but never
# writable destinations. Other users must not be able to redirect our writes.
check_directories() {
  local path=$1 allow_missing=$2 part current= owner mode bits
  local -a parts
  IFS=/ read -r -a parts <<< "$path"
  for part in "${parts[@]}"; do
    [[ -n "$part" ]] || continue
    current+=/$part
    [[ ! -L "$current" ]] || fail "Refusing symlink directory: $current"
    if [[ ! -e "$current" ]]; then
      [[ "$allow_missing" == true ]] || fail "Directory does not exist: $current; create a private backup directory first."
      continue
    fi
    [[ -d "$current" ]] || fail "Not a directory: $current"
    read -r owner mode <<< "$(stat -c '%u %a' -- "$current")"
    [[ "$owner" == "$UID" || "$owner" == 0 ]] || fail "Directory belongs to another user: $current"
    bits=$((8#$mode))
    if (( bits & 0022 )); then
      [[ "$owner" == 0 ]] && (( bits & 01000 )) || fail "Directory is group/world-writable: $current"
    fi
  done
}

check_destination_directory() {
  local path=$1 owner mode
  check_directories "$path" false
  read -r owner mode <<< "$(stat -c '%u %a' -- "$path")"
  [[ "$owner" == "$UID" ]] || fail "Destination directory must belong to the current user: $path"
  (( (8#$mode & 0022) == 0 )) || fail "Destination directory must not be group/world-writable: $path"
}

require_binary() {
  local path=$1 owner links mode
  [[ ! -L "$path" && -f "$path" && -x "$path" ]] || fail "Expected a regular, non-symlink executable: $path"
  read -r owner links mode <<< "$(stat -c '%u %h %a' -- "$path")"
  [[ "$owner" == "$UID" ]] || fail "Executable belongs to another owner: $path; use that owner's maintenance procedure."
  [[ "$links" == 1 ]] || fail "Refusing multiply linked executable: $path"
  (( (8#$mode & 06022) == 0 )) || fail "Refusing privileged or group/world-writable executable: $path"
}

file_hash() {
  local result
  result=$(sha256sum < "$1") || fail "Cannot read executable: $1"
  REPLY=${result%% *}
}

absolute_path "$prefix"
prefix=$REPLY
bin_dir=${prefix%/}/bin
target=$bin_dir/cantrip
check_directories "$bin_dir" true
if [[ "$backup_seen" == true ]]; then
  absolute_path "$backup"
  backup=$REPLY
  backup_dir=$(dirname -- "$backup")
  check_destination_directory "$backup_dir"
  [[ "$backup" != "$target" ]] || fail 'The backup must be a separate path from the installed executable.'
fi

case "$operation" in
  install)
    [[ ! -e "$target" && ! -L "$target" ]] || fail "Target already exists: $target; inspect its owner, then use update with a new backup path."
    ;;
  *) require_binary "$target" ;;
esac
if [[ "$operation" == update ]]; then
  [[ ! -e "$backup" && ! -L "$backup" ]] || fail "Backup path is occupied: $backup; choose a new filename. Nothing was overwritten."
elif [[ "$operation" == rollback ]]; then
  require_binary "$backup"
  [[ ! "$backup" -ef "$target" ]] || fail 'The backup and installed executable must be different files.'
fi

source=
source_hash=
case "$operation" in
  install|update)
    bundle=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
    source=$bundle/cantrip
    [[ ! -L "$source" && -f "$source" && -x "$source" ]] || fail "Bundle executable is missing or unsafe: $source; extract a verified release archive."
    [[ ! -L "$bundle/checksums.txt" && -f "$bundle/checksums.txt" ]] || fail 'Bundle checksums.txt is missing or unsafe; extract a verified release archive.'
    (cd -- "$bundle" && sha256sum --check --strict --status checksums.txt) || fail 'Bundle checksum verification failed; do not install this archive. Download and verify it again.'
    checksum_pattern='^([0-9a-fA-F]{64}) [ *]cantrip$'
    while IFS= read -r line || [[ -n "$line" ]]; do
      if [[ "$line" =~ $checksum_pattern ]]; then
        [[ -z "$source_hash" ]] || fail 'Bundle checksums.txt lists cantrip more than once.'
        source_hash=${BASH_REMATCH[1],,}
      fi
    done < "$bundle/checksums.txt"
    [[ -n "$source_hash" ]] || fail 'Bundle checksums.txt does not cover cantrip.'
    ;;
  rollback)
    source=$backup
    file_hash "$source"
    source_hash=$REPLY
    ;;
esac

# A failed ping is not proof of shutdown. Inspect the selected runtime socket
# and destination executable without running the old or downloaded binary.
# Other prefixes and sessions are independent. Stale sockets stay untouched.
runtime_base=${XDG_RUNTIME_DIR:-/tmp/cantrip-$UID}
runtime_socket=${runtime_base%/}/cantrip/cantrip.sock
assert_stopped() {
  local number refs protocol flags type state inode socket_path process executable
  [[ -r /proc/net/unix && -r /proc/self/cmdline ]] || fail 'Cannot inspect Linux /proc; daemon shutdown cannot be verified.'
  while read -r number refs protocol flags type state inode socket_path; do
    if [[ "$socket_path" == "$runtime_socket" || "$socket_path" -ef "$runtime_socket" ]]; then
      fail "A live Cantrip socket exists at $socket_path. Finish/cancel the take, stop the existing owner, and wait for exit. 'cantrip stop' only ends recording."
    fi
  done < /proc/net/unix
  for process in /proc/[0-9]*; do
    [[ -O "$process" ]] || continue
    executable=$(readlink -- "$process/exe" 2>/dev/null) || continue
    if [[ "$process/exe" -ef "$target" || "$executable" == "$target" || "$executable" == "$target (deleted)" ]]; then
      fail "Installed executable is still running (PID ${process##*/}: $executable). Stop it gracefully through its existing owner and wait for exit; do not use broad process killing or delete sockets."
    fi
  done
}

assert_stopped
original_identity=
original_hash=
if [[ "$operation" != install ]]; then
  original_identity=$(stat -c '%d:%i' -- "$target")
  file_hash "$target"
  original_hash=$REPLY
fi

lock=$bin_dir/.cantrip-install.lock
lock_owned=false
backup_stage=
cleanup() {
  if [[ -n "$backup_stage" ]]; then
    rm -f -- "$backup_stage/cantrip"
    rmdir -- "$backup_stage"
  fi
  if [[ "$lock_owned" == true ]]; then
    rm -f -- "$lock/cantrip"
    rmdir -- "$lock"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
mkdir -p -- "$bin_dir" || fail "Cannot create binary directory: $bin_dir"
check_destination_directory "$bin_dir"
mkdir -- "$lock" 2>/dev/null || fail "Maintenance lock already exists or cannot be created: $lock. Wait for the other installer; after an interrupted run, inspect the directory and confirm no installer is running before removing only that lock."
lock_owned=true

assert_target_unchanged() {
  check_destination_directory "$bin_dir"
  if [[ "$operation" == install ]]; then
    [[ ! -e "$target" && ! -L "$target" ]] || fail "Target appeared during installation: $target; nothing was replaced."
  else
    require_binary "$target"
    [[ "$(stat -c '%d:%i' -- "$target")" == "$original_identity" ]] || fail "Executable changed during maintenance: $target; nothing was replaced."
    file_hash "$target"
    [[ "$REPLY" == "$original_hash" ]] || fail "Executable contents changed during maintenance: $target; nothing was replaced."
  fi
}
assert_target_unchanged

if [[ "$operation" == uninstall ]]; then
  assert_stopped
  rm -- "$target" || fail "Cannot remove executable: $target"
  printf 'Removed %s. Startup owners, backups, and all operator data were left unchanged.\n' "$target"
  exit 0
fi

install -m755 -- "$source" "$lock/cantrip" || fail 'Cannot stage executable; the installed binary has not been replaced.'
file_hash "$lock/cantrip"
[[ "$REPLY" == "$source_hash" ]] || fail 'Staged executable checksum mismatch; the installed binary has not been replaced.'

if [[ "$operation" == update ]]; then
  check_destination_directory "$backup_dir"
  assert_target_unchanged
  # Publish a complete private backup with link(2), which cannot overwrite an
  # occupied pathname. Stage beside the backup, allowing a different filesystem.
  backup_stage=$(mktemp -d -- "$backup_dir/.cantrip-backup.XXXXXXXX") || fail "Cannot stage backup in $backup_dir; the installed binary has not been replaced."
  install -m700 -- "$target" "$backup_stage/cantrip" || fail 'Cannot copy backup; the installed binary has not been replaced.'
  file_hash "$backup_stage/cantrip"
  [[ "$REPLY" == "$original_hash" ]] || fail 'Backup checksum mismatch; the installed binary has not been replaced.'
  ln -T -- "$backup_stage/cantrip" "$backup" || fail "Cannot publish backup at $backup; choose an unoccupied path. The installed binary has not been replaced."
  rm -- "$backup_stage/cantrip"
  rmdir -- "$backup_stage"
  backup_stage=
  printf 'Retained previous executable at %s\n' "$backup"
fi

assert_target_unchanged
assert_stopped
if [[ "$operation" == install ]]; then
  ln -T -- "$lock/cantrip" "$target" || fail "Cannot install at $target; an existing target is never overwritten."
else
  mv -T -- "$lock/cantrip" "$target" || fail "Atomic replacement failed at $target; the previous executable and any published backup remain available."
fi
printf '%s complete: %s\nNo daemon or startup owner was started or changed.\n' "$operation" "$target"
