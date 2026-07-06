#!/bin/bash
cd /Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f
run() {
  local slug=$(echo "$1 $2" | tr '/ .' '___')
  python3 scratch_diag.py diag "$1" "$2" > scratch_out/$slug.txt 2>&1
  echo "DONE $1 $2"
}
runrepro() {
  local slug=$(echo "$1 $2" | tr '/ .' '___')
  python3 scratch_diag.py repro "$1" "$2" > scratch_out/repro_$slug.txt 2>&1
  echo "DONE repro $1 $2"
}
run elmcraft/core-extra 2.3.0
run arowM/elm-markdown-ast 1.0.6
run PaackEng/paack-ui 7.23.0
run intrepidshape/elm-web3-ui 2.3.0
runrepro gicentre/tidy 1.6.0
runrepro terezka/intervals 2.0.2
echo ALLDONE2
