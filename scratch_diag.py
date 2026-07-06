import sys, os
BASE = "/Users/boxed/Projects/alm/.registry/tests"
sys.path.insert(0, BASE)
import suite_harness as H
H.ALM = "/Users/boxed/Projects/alm/.claude/worktrees/agent-a79f787c140bb5b7f/target/debug/alm"
which = sys.argv[1]  # 'diag' or 'repro'
sys.argv = [which] + sys.argv[2:]
exec(open(os.path.join(BASE, which + ".py")).read())
