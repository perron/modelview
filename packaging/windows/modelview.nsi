; Unsigned per-user installer. The portable zip is the artifact that matters;
; this exists so a double-click install is possible without an admin prompt.
!include "MUI2.nsh"

!ifndef SOURCE
  !define SOURCE "modelview.exe"
!endif

Name "modelview 0.1.0"
OutFile "modelview-0.1.0-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\modelview"
RequestExecutionLevel user
Unicode True

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "modelview"
  SetOutPath $INSTDIR
  File "${SOURCE}"
  CreateShortcut "$SMPROGRAMS\modelview.lnk" "$INSTDIR\modelview.exe"
  ; The installer does not edit PATH. The exe is at %LOCALAPPDATA%\modelview\modelview.exe.
SectionEnd
