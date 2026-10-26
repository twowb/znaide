; 修凡AI全能助手 Windows 安装包(NSIS 3 / Unicode)。
; 由 Makefile 的 package-gui-windows-installer 调用,路径与版本用 -D 传入:
;   -DOUTFILE=... -DVERSION=0.0.1 -DAPP_EXE=... -DICON=... -DLICENSE=...
;
; 两条硬约束(见 docs/发布清单):
; 1) 装到 %LOCALAPPDATA%\Programs\... 而不是 Program Files —— 应用内自更新是原地
;    替换 current_exe(),装进 Program Files 后每次更新都要 UAC,不弹就静默失败。
; 2) 卸载不删 %USERPROFILE%\.znaide(会话/配置/记忆),只询问,默认保留。

Unicode true
SetCompressor /SOLID lzma

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef OUTFILE
  !define OUTFILE "xiufan-windows-x64-setup.exe"
!endif
!ifndef APP_EXE
  !error "缺少 -DAPP_EXE=<znaide-gui.exe 路径>"
!endif
!ifndef ICON
  !define ICON "znaide-gui.ico"
!endif
!ifndef LICENSE
  !define LICENSE "LICENSE"
!endif

!define PRODUCT     "修凡AI全能助手"
!define PRODUCT_KEY "XiufanAIAssistant"
!define PUBLISHER   "twowb"
!define URL_INFO    "https://github.com/twowb/znaide"
!define EXE_NAME    "xiufan.exe"
!define UNINST_KEY  "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_KEY}"
!define WEBVIEW2_GUID "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WEBVIEW2_URL  "https://go.microsoft.com/fwlink/p/?LinkId=2124703"

Name "${PRODUCT}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${PRODUCT}"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
RequestExecutionLevel user
BrandingText "${PRODUCT} v${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=2052 "ProductName" "${PRODUCT}"
VIAddVersionKey /LANG=2052 "FileDescription" "修凡AI全能助手（基于 znaide 引擎）安装程序"
VIAddVersionKey /LANG=2052 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=2052 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "AGPL-3.0"
VIAddVersionKey /LANG=2052 "CompanyName" "${PUBLISHER}"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "StrFunc.nsh"
${StrStr}

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TITLE "安装 ${PRODUCT}"
!define MUI_WELCOMEPAGE_TEXT "将安装 ${PRODUCT} v${VERSION}（基于 znaide 引擎）。$\r$\n$\r$\n安装到当前用户目录,不需要管理员权限;卸载与升级都在“设置 → 应用”里完成。"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE_NAME}"
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 ${PRODUCT}"
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

LangString DESC_Main    ${LANG_SIMPCHINESE} "主程序与卸载信息(必需)。"
LangString DESC_Desktop ${LANG_SIMPCHINESE} "在桌面放一个 ${PRODUCT} 图标。"

; 缺 WebView2 运行时(精简版/Server)会只剩白窗口,先探再提示。
Function CheckWebView2
  ReadRegStr $R0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2_GUID}" "pv"
  ${If} $R0 == ""
    ReadRegStr $R0 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2_GUID}" "pv"
  ${EndIf}
  ${If} $R0 == ""
    ReadRegStr $R0 HKCU "Software\Microsoft\EdgeUpdate\Clients\${WEBVIEW2_GUID}" "pv"
  ${EndIf}
  ${If} $R0 == "0.0.0.0"
    StrCpy $R0 ""
  ${EndIf}
  ${If} $R0 == ""
    ; 静默安装不打扰,交给程序自己报错
    ${IfNot} ${Silent}
      MessageBox MB_YESNO|MB_ICONQUESTION "系统缺少 Microsoft Edge WebView2 运行时，界面会显示不出来。$\r$\n$\r$\n现在打开官方下载页吗？（约 2MB，装完再运行本程序）" IDNO +2
      ExecShell "open" "${WEBVIEW2_URL}"
    ${EndIf}
  ${EndIf}
FunctionEnd

Section "主程序" SecMain
  SectionIn RO

  ; 覆盖安装:exe 被占用会写失败,先把在跑的实例关掉
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ${EXE_NAME}" /NH'
  Pop $0
  Pop $1
  ${StrStr} $2 "$1" "${EXE_NAME}"
  ${If} $2 != ""
    nsExec::Exec 'taskkill /IM ${EXE_NAME} /F'
    Sleep 1000
  ${EndIf}

  SetOutPath "$INSTDIR"
  File "/oname=${EXE_NAME}" "${APP_EXE}"
  File "/oname=LICENSE.txt" "${LICENSE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${PRODUCT}"
  CreateShortCut "$SMPROGRAMS\${PRODUCT}\${PRODUCT}.lnk" "$INSTDIR\${EXE_NAME}"
  CreateShortCut "$SMPROGRAMS\${PRODUCT}\卸载 ${PRODUCT}.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout" "${URL_INFO}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${EXE_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"

  Call CheckWebView2
SectionEnd

Section "桌面快捷方式" SecDesktop
  ; 精简系统/新装用户目录里可能还没有 Desktop,先补目录再建链接
  CreateDirectory "$DESKTOP"
  CreateShortCut "$DESKTOP\${PRODUCT}.lnk" "$INSTDIR\${EXE_NAME}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} $(DESC_Main)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DESC_Desktop)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$INSTDIR\${EXE_NAME}"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${PRODUCT}\${PRODUCT}.lnk"
  Delete "$SMPROGRAMS\${PRODUCT}\卸载 ${PRODUCT}.lnk"
  RMDir "$SMPROGRAMS\${PRODUCT}"
  Delete "$DESKTOP\${PRODUCT}.lnk"

  DeleteRegKey HKCU "${UNINST_KEY}"

  ; 数据目录默认保留:里面是用户的会话、配置、人格与记忆。
  ; 静默卸载(/S)一律保留,只有用户当场点“是”才删。
  ${IfNot} ${Silent}
    MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除配置与会话数据？$\r$\n$\r$\n$PROFILE\.znaide$\r$\n$\r$\n选择“否”将保留，重装后可继续使用。" IDNO keep_data
    RMDir /r "$PROFILE\.znaide"
  ${EndIf}
  keep_data:
SectionEnd
