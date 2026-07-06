cd /Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f
for pv in "terezka/intervals 2.0.2" "mthiems/intervals 1.0.0"; do
  set -- $pv; slug=$(echo "$pv"|tr '/ .' '___')
  python3 scratch_diag.py diag "$1" "$2" > scratch_out/vi_$slug.txt 2>&1
  echo "$pv => PASS=$(grep -c '^PASS' scratch_out/vi_$slug.txt) FAIL=$(grep -c '^FAIL' scratch_out/vi_$slug.txt)"
  grep -E "RUN FAIL|BUILD FAIL" scratch_out/vi_$slug.txt | head -1
  grep '^FAIL' scratch_out/vi_$slug.txt | head -3
done
echo DONE
