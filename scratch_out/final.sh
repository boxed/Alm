cd /Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f
run(){ slug=$(echo "$1 $2"|tr '/ .' '___'); python3 scratch_diag.py diag "$1" "$2" > scratch_out/f_$slug.txt 2>&1; echo "$1 $2 => PASS=$(grep -c '^PASS' scratch_out/f_$slug.txt) FAIL=$(grep -c '^FAIL' scratch_out/f_$slug.txt)"; grep '^FAIL' scratch_out/f_$slug.txt | head -2 | cut -c1-120; }
run scrive/json-schema-form 2.0.0
run PaackEng/paack-ui 7.23.0
run intrepidshape/elm-web3-ui 2.3.0
echo FINALDONE
