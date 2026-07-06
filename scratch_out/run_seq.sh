#!/bin/bash
cd /Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f
run() {
  local slug=$(echo "$1 $2" | tr '/ .' '___')
  python3 scratch_diag.py diag "$1" "$2" > scratch_out/$slug.txt 2>&1
  echo "DONE $1 $2 -> scratch_out/$slug.txt"
}
run lydell/elm-app-url 1.0.4
run scrive/json-schema-form 2.0.0
run anmolitor/elm-protoc-utils 2.3.0
run thought2/elm-wikimedia-commons 1.0.1
run owanturist/elm-graphql 5.0.0
run ozmat/elm-forms 2.0.1
echo ALLDONE
