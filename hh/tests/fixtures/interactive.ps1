# Interactive PTY transparency fixture (FR-1.1), Windows/PowerShell twin of
# interactive.sh.
#
# Proves the wrapped program is indistinguishable from running it directly in
# a terminal: `$Host.UI.RawUI.WindowSize.Width` only resolves on a real
# console, so printing a nonzero column count demonstrates the child runs
# inside a ConPTY even when hh's own stdout is piped. It then prints an
# ANSI-colored line and reads one line from stdin to exercise raw stdin
# forwarding + echo.
try {
    $cols = $Host.UI.RawUI.WindowSize.Width
} catch {
    $cols = 0
}
Write-Output "cols=$cols"
$esc = [char]27
Write-Output "$esc[32mgreen-line$esc[0m"
$line = [Console]::In.ReadLine()
Write-Output "echo:$line"
