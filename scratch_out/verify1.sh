#!/bin/bash
cd /Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f
run() {
  local slug=$(echo "$1 $2" | tr '/ .' '___')
  python3 scratch_diag.py diag "$1" "$2" > scratch_out/v_$slug.txt 2>&1
  echo "$1 $2 => PASS=$(grep -c '^PASS' scratch_out/v_$slug.txt) FAIL=$(grep -c '^FAIL' scratch_out/v_$slug.txt)"
  grep '^FAIL' scratch_out/v_$slug.txt | head -3
}
run anmolitor/elm-protoc-utils 2.3.0
run elmcraft/core-extra 2.3.0
run owanturist/elm-graphql 5.0.0
run ozmat/elm-forms 2.0.1
echo VDONE
