' #81: launch start-stream.cmd without a console window.
'
' The scheduled task fires every minute as a watchdog. Running cmd.exe directly
' would flash a console on the interactive desktop at every fire, which is both
' ugly and a way to lose focus in a fullscreen game. Window style 0 = hidden,
' bWaitOnReturn = True keeps the task in the Running state for as long as the
' server lives — that is what makes Task Scheduler's "do not start a new
' instance" policy turn into a watchdog instead of a minute-by-minute respawn.
Dim sh
Set sh = CreateObject("WScript.Shell")
sh.Run "cmd /c ""C:\dev\stream\start-stream.cmd""", 0, True
