# Fake agent fixture for `hh run` integration tests (FR-1). Windows/PowerShell
# twin of fake_agent.py -- see that file for the full rationale (sleep gaps
# so each change kind lands as a distinct file_change row, ignored-path
# writes, exit 3). Kept behaviorally identical so the same assertions in
# cli.rs hold on both platforms.
# [char]27 (not the `e` backtick escape) so this runs on both Windows
# PowerShell 5.1 and PowerShell 7+ (`e` is 7.0+ only).
$esc = [char]27
Write-Output "$esc[31magent-start$esc[0m hello from fake agent"
Start-Sleep -Milliseconds 200

# `-NoNewline` + an explicit `` `n `` gives an exact `\n`-terminated byte
# string on both PowerShell 5.1 and 7+ -- `Set-Content` without `-NoNewline`
# appends the platform line terminator (`\r\n` on Windows), which would break
# `assert_lifecycle_changes`'s exact-byte blob-content assertions shared with
# the Unix (`fake_agent.py`) fixture. `-Encoding ascii` avoids a BOM.

# A file we will delete later, created early so create/delete are well
# separated in time.
Set-Content -Path doomed.txt -Value "bye-bye`n" -NoNewline -Encoding ascii
Start-Sleep -Seconds 2

# A brand-new file: exercises `created`.
Set-Content -Path created.txt -Value "created-content`n" -NoNewline -Encoding ascii
Start-Sleep -Milliseconds 200

# A pre-existing file (the test seeds "modified.txt" before launching hh)
# gets overwritten: exercises `modified`.
Set-Content -Path modified.txt -Value "modified-content`n" -NoNewline -Encoding ascii
Start-Sleep -Milliseconds 200

# Delete the file created at the start: exercises `deleted`.
Remove-Item -Path doomed.txt
Start-Sleep -Milliseconds 200

# Ignored paths -- built-in ignores (`.git/`, `target/`); must produce NO
# file_change events.
New-Item -ItemType Directory -Path target -Force | Out-Null
Set-Content -Path target/ignored.txt -Value "nope`n" -NoNewline -Encoding ascii
New-Item -ItemType Directory -Path .git -Force | Out-Null
Set-Content -Path .git/config -Value "nope`n" -NoNewline -Encoding ascii

Write-Output "done"
exit 3
