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
