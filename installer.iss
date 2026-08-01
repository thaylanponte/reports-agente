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
Name: "{commonappdata}\Report Studio Agent\logs"; Permissions: users-modify

[Files]
Source: "target\x86_64-pc-windows-gnu\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\x86_64-pc-windows-gnu\release\{#ServiceExeName}"; DestDir: "{app}"; Flags: ignoreversion

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

{ Coleta as linhas do agente.toml existente que NAO sao as tres chaves geridas
  por este instalador, devolvendo-as verbatim (comentarios e espacamento
  inclusive) para serem reanexadas ao arquivo novo.

  Motivo: CurStepChanged reescreve o arquivo do zero e SaveStringToFile com
  False sobrescreve. O agente 0.5.0 aceita max_consultas e max_exportacoes no
  mesmo arquivo, ajustaveis a mao quando o DB2 de um cliente esta sofrendo — e
  sao os unicos parametros do agente que afetam o sistema de caixa da loja. Sem
  esta funcao, essa valvula so funcionaria ate a proxima atualizacao.

  LIMITACAO CONHECIDA: a deteccao usa Pos(chave, L) = 1, a mesma convencao de
  LerConfigExistente. Uma chave futura que COMECE com 'servidor_url',
  'empresa_id' ou 'token' — 'token_backup', por exemplo — seria considerada
  gerida por este instalador e portanto descartada. A convencao foi mantida por
  coerencia com o resto do arquivo; se surgir uma chave assim, os dois lugares
  precisam mudar juntos. }
function LerLinhasExtras(): String;
var
  ConfigFile: String;
  Linhas: TArrayOfString;
  i: Integer;
  L: String;
begin
  Result := '';
  ConfigFile := ExpandConstant('{commonappdata}\Report Studio Agent\agente.toml');
  if not LoadStringsFromFile(ConfigFile, Linhas) then
    Exit;
  for i := 0 to GetArrayLength(Linhas) - 1 do
  begin
    L := Trim(Linhas[i]);
    if Pos('servidor_url', L) = 1 then Continue;
    if Pos('empresa_id', L) = 1 then Continue;
    if Pos('token', L) = 1 then Continue;
    { Linha original, nao a trimada: preserva indentacao e alinhamento. }
    Result := Result + Linhas[i] + #13#10;
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
  ConfigDir, ConfigFile, Conteudo, Extras: String;
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
    { Le ANTES de sobrescrever: preserva max_consultas, max_exportacoes e
      qualquer comentario que o operador tenha deixado no arquivo. }
    Extras := LerLinhasExtras();
    Conteudo :=
      'servidor_url = "' + Url + '"' + #13#10 +
      'empresa_id   = "' + Empresa + '"' + #13#10 +
      'token        = "' + Token + '"' + #13#10 +
      Extras;
    SaveStringToFile(ConfigFile, Conteudo, False);
  end;

  // Instala e inicia o servico (delayed auto-start + failure actions sao
  // configurados dentro do proprio 'install').
  if WizardIsTaskSelected('installservice') then
  begin
    if not Exec(ExpandConstant('{app}\{#ServiceExeName}'), 'install', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
      ResultCode := -1;

    { O 'install' configura failureflag, acoes de restart e delayed-auto. Quando
      alguma dessas falha, ele imprime um banner no console — que SW_HIDE torna
      invisivel. Sem checar o codigo aqui, o instalador reportaria sucesso e o
      cliente ficaria sem recuperacao automatica sem ninguem saber.

      Nao aborta a instalacao de proposito: o binario ja esta no lugar e o
      agente funciona. O que pode faltar e so o levantar-se sozinho apos uma
      queda. }
    if ResultCode <> 0 then
      MsgBox('O servico foi instalado, mas o comando retornou codigo ' + IntToStr(ResultCode) + '.' + #13#10 + #13#10 +
             'O agente funciona normalmente, porem a recuperacao automatica pode nao ter sido ' +
             'configurada: se o servico cair, o Windows pode nao levanta-lo sozinho.' + #13#10 + #13#10 +
             'Detalhes em:' + #13#10 +
             ExpandConstant('{commonappdata}\Report Studio Agent\logs'),
             mbError, MB_OK);

    Exec(ExpandConstant('{app}\{#ServiceExeName}'), 'start', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;
