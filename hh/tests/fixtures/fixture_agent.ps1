# Fixture fake agent for `hh run` integration tests (SRS acceptance #2).
#
# PowerShell variant, exercising portable-pty's ConPTY backend with a native
# Windows shell (the .py variant covers the same flow through a Python child).
# Behavior must stay in lockstep with fixture_agent.sh and fixture_agent.py.
Write-Output "agent starting"
Write-Output "writing a file"
Set-Content -Path "fixture_output.txt" -Value "hello from fixture" -Encoding utf8
Write-Output "writing again"
Set-Content -Path "fixture_output.txt" -Value "hello v2 from fixture" -Encoding utf8
Write-Output "now exiting with error"
exit 3
