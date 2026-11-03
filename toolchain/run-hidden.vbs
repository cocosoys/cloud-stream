' #81: launch start-stream.cmd without a console window.
'
' PORTABLE: the launcher path is resolved from this script's own location, not
' from a hardcoded C:\dev\stream. The scheduled task stores
' "wscript.exe <dir>\run-hidden.vbs", so as long as the bundle stays together the
' task keeps working after the directory is moved or the bundle is unpacked on a
' different machine.
'
' The scheduled task fires every minute as a watchdog. Running cmd.exe directly
' would flash a console on the interactive desktop at every fire, which is both
' ugly and a way to lose focus in a fullscreen game. Window style 0 = hidden,
' bWaitOnReturn = True keeps the task in the Running state for as long as the
' server lives - that is what makes Task Scheduler's "do not start a new
' instance" policy turn into a watchdog instead of a minute-by-minute respawn.
Option Explicit

Dim sh, fso, dir, cmd
Set fso = CreateObject("Scripting.FileSystemObject")
Set sh = CreateObject("WScript.Shell")

dir = fso.GetParentFolderName(WScript.ScriptFullName)
cmd = """" & fso.BuildPath(dir, "start-stream.cmd") & """"

' Inherit the directory the task starts in: start-stream.cmd derives everything
' from %~dp0 anyway, but a stale working directory is a classic source of
' "it worked by hand and not from the scheduler".
sh.CurrentDirectory = dir
sh.Run "cmd /c " & cmd, 0, True
