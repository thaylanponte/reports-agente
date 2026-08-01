; ============================================================================
; Report Studio Agent - Instalador
; Requer Inno Setup 6+ (https://jrsoftware.org/isinfo.php)
;
; Como usar:
;   1. cargo build --release  (ou cross-compile p/ x86_64-pc-windows-gnu)
;   2. Abrir este .iss no Inno Setup Compiler / ISCC
;   3. Compilar
;   4. O instalador sera gerado em .\Output\ReportStudioAgentSetup.exe
;
; O instalador pede URL/Empresa/Token numa tela, grava o agente.toml e ja
; instala e inicia o servico. Nao precisa configurar nada na mao depois.
; ============================================================================

#define AppName "Report Studio Agent"
#define AppVersion "0.5.0"
#define AppPublisher "Guidetti Consultoria"
#define AppExeName "agente.exe"
#define ServiceExeName "agente-service.exe"
#define DefaultUrl "wss://reports.orizoncloud.com.br/ws/agente"

[Setup]
AppId={{B7E2F1A0-4C3D-4E5F-8A1B-9C0D2E3F4A5B}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\Report Studio Agent
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=.\Output
OutputBaseFilename=ReportStudioAgentSetup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
PrivilegesRequired=admin
UninstallDisplayIcon={app}\{#AppExeName}
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Dirs]
Name: "{commonappdata}\Report Studio Agent"; Permissions: users-modify

[Files]
Source: "target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\release\{#ServiceExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Criar atalho na Area de Trabalho"
Name: "installservice"; Description: "Instalar e iniciar o servico Windows (recomendado)"

[UninstallRun]
Filename: "{app}\{#ServiceExeName}"; Parameters: "stop"; RunOnceId: "StopSvc"; Flags: runhidden waituntilterminated
Filename: "{app}\{#ServiceExeName}"; Parameters: "uninstall"; RunOnceId: "DelSvc"; Flags: runhidden waituntilterminated

[UninstallDelete]
Type: files; Name: "{commonappdata}\Report Studio Agent\agente.toml"
Type: dirifempty; Name: "{commonappdata}\Report Studio Agent"

[Code]
var
  PaginaConfig: TInputQueryWizardPage;

function ExtrairValor(const Linha: String): String;
var
  p1, p2: Integer;
  Resto: String;
begin
  Result := '';
  p1 := Pos('"', Linha);
  if p1 = 0 then Exit;
  Resto := Copy(Linha, p1 + 1, Length(Linha));
  p2 := Pos('"', Resto);
  if p2 = 0 then Exit;
  Result := Copy(Resto, 1, p2 - 1);
end;

procedure LerConfigExistente(var Url, Empresa, Token: String);
var
  ConfigFile: String;
  Linhas: TArrayOfString;
  i: Integer;
  L: String;
begin
  ConfigFile := ExpandConstant('{commonappdata}\Report Studio Agent\agente.toml');
  if not LoadStringsFromFile(ConfigFile, Linhas) then
    Exit;
  for i := 0 to GetArrayLength(Linhas) - 1 do
  begin
    L := Trim(Linhas[i]);
    if Pos('servidor_url', L) = 1 then
      Url := ExtrairValor(L)
    else if Pos('empresa_id', L) = 1 then
      Empresa := ExtrairValor(L)
    else if Pos('token', L) = 1 then
      Token := ExtrairValor(L);
  end;
end;

procedure InitializeWizard;
var
  Url, Empresa, Token: String;
begin
  PaginaConfig := CreateInputQueryPage(
    wpSelectTasks,
    'Configuracao do Agente',
    'Dados de conexao fornecidos pelo painel administrativo.',
    'Informe o ID da Empresa e o Token do Agente. Os dados sao gravados na configuracao e o servico inicia automaticamente.');
  PaginaConfig.Add('URL do Servidor:', False);
  PaginaConfig.Add('ID da Empresa:', False);
  PaginaConfig.Add('Token do Agente:', False);

  // Prefill: usa o que ja estiver instalado; senao, a URL padrao.
  Url := '{#DefaultUrl}';
  Empresa := '';
  Token := '';
  LerConfigExistente(Url, Empresa, Token);
  if Trim(Url) = '' then
    Url := '{#DefaultUrl}';

  PaginaConfig.Values[0] := Url;
  PaginaConfig.Values[1] := Empresa;
  PaginaConfig.Values[2] := Token;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = PaginaConfig.ID then
  begin
    if WizardIsTaskSelected('installservice') then
    begin
      if (Trim(PaginaConfig.Values[1]) = '') or (Trim(PaginaConfig.Values[2]) = '') then
      begin
        MsgBox('Para instalar o servico, preencha o ID da Empresa e o Token do Agente.', mbError, MB_OK);
        Result := False;
      end;
    end;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';

  // Para e remove o servico da instalacao anterior usando o exe ainda presente.
  if FileExists(ExpandConstant('{app}\{#ServiceExeName}')) then
  begin
    Exec(ExpandConstant('{app}\{#ServiceExeName}'), 'stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Exec(ExpandConstant('{app}\{#ServiceExeName}'), 'uninstall', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Sleep(2000);
  end;

  // Remove explicitamente o servico de nome antigo (Guidetti), caso ainda exista.
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop GuidettiAnalyticsAgent', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\sc.exe'), 'delete GuidettiAnalyticsAgent', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ConfigDir, ConfigFile, Conteudo: String;
  Url, Empresa, Token: String;
  ResultCode: Integer;
begin
  if CurStep <> ssPostInstall then
    Exit;

  Url := Trim(PaginaConfig.Values[0]);
  Empresa := Trim(PaginaConfig.Values[1]);
  Token := Trim(PaginaConfig.Values[2]);

  if Url = '' then
    Url := '{#DefaultUrl}';

  // Grava a config preenchida no assistente (so se tiver empresa + token).
  if (Empresa <> '') and (Token <> '') then
  begin
    ConfigDir := ExpandConstant('{commonappdata}\Report Studio Agent');
    ForceDirectories(ConfigDir);
    ConfigFile := ConfigDir + '\agente.toml';
    Conteudo :=
      'servidor_url = "' + Url + '"' + #13#10 +
      'empresa_id   = "' + Empresa + '"' + #13#10 +
      'token        = "' + Token + '"' + #13#10;
    SaveStringToFile(ConfigFile, Conteudo, False);
  end;

  // Instala e inicia o servico (delayed auto-start + failure actions sao
  // configurados dentro do proprio 'install').
  if WizardIsTaskSelected('installservice') then
  begin
    Exec(ExpandConstant('{app}\{#ServiceExeName}'), 'install', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Exec(ExpandConstant('{app}\{#ServiceExeName}'), 'start', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;
