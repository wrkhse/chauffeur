#!/usr/bin/env bash
# Frame text wraps at narrow pane widths, splitting a needle such as
# "command":"git status" across lines with indentation inserted. Compare
# the non-whitespace characters of both sides so a correct display still
# matches after a wrap.
frame_matches() {
  local frame=$1 needle=$2
  local flat_frame flat_needle

  flat_frame=$(tr -d '[:space:]' <<< "$frame")
  flat_needle=$(tr -d '[:space:]' <<< "$needle")

  [[ $flat_frame == *"$flat_needle"* ]]
}

# An echoed terminal control is a whole line reading go or stop. A
# flattened substring would also match a caption such as "goes to", so
# split the raw OCR text into lines, strip each line's whitespace and
# compare the whole line, ignoring case.
has_control_line() {
  local text=$1 line flat

  while IFS= read -r line; do
    flat=$(printf '%s' "$line" \
      | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')
    case $flat in
      go|stop)
        return 0
        ;;
    esac
  done <<< "$text"

  return 1
}
