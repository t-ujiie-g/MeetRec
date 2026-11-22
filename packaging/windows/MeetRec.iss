; MeetRec の Windows インストーラ（Inno Setup 6）。
;   iscc /DAppVersion=0.1.0 packaging\windows\MeetRec.iss
; dist\MeetRec.exe を、利用者ごとのフォルダ（%LOCALAPPDATA%\Programs\MeetRec）へ入れる。管理者権限は使わない。
; このファイルは BOM 付き UTF-8 で保存する（日本語の文言を含むため）。

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
; AppId は更新とアンインストールの対応付けに使う。変えないこと。
AppId={{6C0F4A52-5E0B-4B56-9B0E-3B2B6D1E7A41}
AppName=MeetRec
AppVersion={#AppVersion}
AppPublisher=MeetRec
DefaultDirName={localappdata}\Programs\MeetRec
PrivilegesRequired=lowest
DisableDirPage=yes
DisableProgramGroupPage=yes
OutputDir=..\..\dist
OutputBaseFilename=MeetRec-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=MeetRec
UninstallDisplayIcon={app}\MeetRec.exe
; 更新のとき、動いている MeetRec を閉じてから入れ替える（録画中なら確定してから終了する）。
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[Files]
Source: "..\..\dist\MeetRec.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\MeetRec"; Filename: "{app}\MeetRec.exe"

[Registry]
; 「ログイン時に起動」は MeetRec 自身が登録する（Run キーの MeetRec）。アンインストール時に消す。
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "MeetRec"; ValueType: none; Flags: dontcreatekey uninsdeletevalue

[Run]
Filename: "{app}\MeetRec.exe"; Description: "MeetRec を起動する"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; まず通常の終了を頼み（録画中なら確定される）、残っていれば強制終了する。
Filename: "{sys}\taskkill.exe"; Parameters: "/IM MeetRec.exe"; Flags: runhidden; RunOnceId: "CloseMeetRec"
Filename: "{sys}\taskkill.exe"; Parameters: "/IM MeetRec.exe /F"; Flags: runhidden; RunOnceId: "KillMeetRec"
