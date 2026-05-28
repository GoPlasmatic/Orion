# Shared rendering helpers for the documentation demo recordings.
#
# Each `step` prints a typed-looking prompt + command, then runs it for real,
# so the recording reflects genuine server output (no faked terminals).
# Sourced by the demo-*.sh scripts; not meant to run on its own.

_C_PROMPT=$'\033[1;36m'   # bold cyan prompt
_C_RESET=$'\033[0m'
_C_NOTE=$'\033[2;37m'     # dim grey comment

TYPE_DELAY="${TYPE_DELAY:-0.015}"
STEP_PAUSE="${STEP_PAUSE:-0.9}"
NOTE_PAUSE="${NOTE_PAUSE:-0.8}"

note() {
  printf '%s# %s%s\n' "$_C_NOTE" "$*" "$_C_RESET"
  sleep "$NOTE_PAUSE"
}

# step "<command line>" — render the command as if typed, then eval it.
# Pass the command as a single argument; use \" for JSON double-quotes and
# literal '...' around JSON bodies so the printed line stays readable.
step() {
  local cmd="$*" i
  printf '%s❯ %s' "$_C_PROMPT" "$_C_RESET"
  for (( i = 0; i < ${#cmd}; i++ )); do
    printf '%s' "${cmd:i:1}"
    sleep "$TYPE_DELAY"
  done
  printf '\n'
  sleep 0.2
  eval "$cmd"
  sleep "$STEP_PAUSE"
}
