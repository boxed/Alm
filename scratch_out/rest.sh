cd /Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f
run() { local slug=$(echo "$1 $2"|tr '/ .' '___'); python3 scratch_diag.py diag "$1" "$2" > scratch_out/r_$slug.txt 2>&1; echo "$1 => PASS=$(grep -c '^PASS' scratch_out/r_$slug.txt) FAIL=$(grep -c '^FAIL' scratch_out/r_$slug.txt)"; }
run ozmat/elm-forms 2.0.1
run intrepidshape/elm-web3-ui 2.3.0
run arowM/elm-markdown-ast 1.0.6
run scrive/json-schema-form 2.0.0
run PaackEng/paack-ui 7.23.0
run jfmengels/elm-review-simplify 2.1.15
echo RESTDONE
