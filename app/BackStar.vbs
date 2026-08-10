' BackStar launcher - starts BackStar.ps1 with NO console window.
' Double-click this file (or a shortcut to it) instead of running the .ps1/.bat directly.
Set fso = CreateObject("Scripting.FileSystemObject")
launcherDir = fso.GetParentFolderName(WScript.ScriptFullName)
CreateObject("WScript.Shell").Run _
    "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File """ & launcherDir & "\BackStar.ps1""", _
    0, False
