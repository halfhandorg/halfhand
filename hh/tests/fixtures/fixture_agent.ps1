# Fixture fake agent for `hh run` integration tests (SRS acceptance #2).
# Windows/PowerShell twin of fixture_agent.sh: prints to stdout, writes a file
# to its cwd twice, and exits nonzero -- enough to exercise terminal-output
# capture, file-change capture, and the error status path on Windows' ConPTY
# backend (portable-pty).
Write-Output "agent starting"
Write-Output "writing a file"
Set-Content -Path fixture_output.txt -Value "hello from fixture"
Write-Output "writing again"
Set-Content -Path fixture_output.txt -Value "hello v2 from fixture"
Write-Output "now exiting with error"
exit 3
