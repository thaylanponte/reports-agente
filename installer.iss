; ============================================================================
; Report Studio Agent - Instalador
; Requer Inno Setup 6+ (https://jrsoftware.org/isinfo.php)
;
; Como usar:
;   1. cargo build --release        (alvo nativo x86_64-pc-windows-msvc)
;   2. Abrir este .iss no Inno Setup Compiler / ISCC
;   3. Compilar
;   4. O instalador sera gerado em .\Output\
;
; O alvo x86_64-pc-windows-gnu foi abandonado: ele exige o MinGW instalado
; (dlltool.exe), que o `rustup target add` NAO traz, e o alvo nativo compila
; tudo, odbc-api inclusive. Nao ha subpasta de alvo no caminho abaixo porque
; build sem --target escreve direto em target\release\.
;
; O instalador pede URL/Empresa/Token numa tela, grava o agente.toml e ja
; instala e inicia o servico. Nao precisa configurar nada na mao depois.
; ============================================================================

#define AppName "Report Studio Agent"
#define AppVersion "0.6.0"
#define AppPublisher "Guidetti Consultoria"
#define AppExeName "agente.exe"
#define ServiceExeName "agente-service.exe"
#define DefaultUrl "wss://reports.orizoncloud.com.br/ws/agente"
#define DefaultPastaIntegracoes "C:\eSales\Envio"

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
Name: "{commonappdata}\Report Studio Agent"
Name: "{commonappdata}\Report Studio Agent\logs"

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

{ Escapa uma string para string basica do TOML (aspas duplas). A ordem importa:
  a barra invertida primeiro, senao a barra que o escape da aspa acrescenta
  seria duplicada tambem.

  Isto existe porque o TOML processa escape dentro de aspas duplas, e um caminho
  do Windows e' quase todo barra invertida. Gravar pasta_integracoes = "C:\eSales"
  produz TOML INVALIDO: o parse falha, carregar_config cai em unwrap_or_default,
  a config inteira some e o agente nao sobe. Quem editou o arquivo a mao nao tem
  como suspeitar da barra. E' o motivo de este campo estar no wizard.

  Espelha shared.rs:salvar_config, que faz o mesmo par de replaces. }
function EscaparToml(const S: String): String;
begin
  Result := S;
  StringChangeEx(Result, '\', '\\', True);
  StringChangeEx(Result, '"', '\"', True);
end;

{ Desfaz EscaparToml numa passada so'. Um scan e nao dois replaces: fazer o
  inverso com replaces exigiria a ordem oposta e ainda erraria em sequencias
  como \\" . }
function DesescaparToml(const S: String): String;
var
  i: Integer;
begin
  Result := '';
  i := 1;
  while i <= Length(S) do
  begin
    if (S[i] = '\') and (i < Length(S)) then
    begin
      Result := Result + S[i + 1];
      i := i + 2;
    end
    else
    begin
      Result := Result + S[i];
      i := i + 1;
    end;
  end;
end;

{ Le o valor de pasta_integracoes aceitando as DUAS formas validas de TOML:
  literal 'C:\eSales\Envio' (a que um humano escreve, sem escape) e basica
  "C:\\eSales\\Envio" (a que este instalador e a GUI gravam).

  Sem aceitar as duas, uma pasta configurada a mao com aspas simples seria lida
  como vazia e a proxima atualizacao a apagaria em silencio. }
function ExtrairCaminho(const Linha: String): String;
var
  pSimples, pDuplas, fim: Integer;
  Resto: String;
begin
  Result := '';
  pSimples := Pos('''', Linha);
  pDuplas := Pos('"', Linha);

  if (pSimples > 0) and ((pDuplas = 0) or (pSimples < pDuplas)) then
  begin
    Resto := Copy(Linha, pSimples + 1, Length(Linha));
    fim := Pos('''', Resto);
    if fim = 0 then Exit;
    Result := Copy(Resto, 1, fim - 1);
  end
  else if pDuplas > 0 then
  begin
    Resto := Copy(Linha, pDuplas + 1, Length(Linha));
    fim := Pos('"', Resto);
    if fim = 0 then Exit;
    Result := DesescaparToml(Copy(Resto, 1, fim - 1));
  end;
end;

procedure LerConfigExistente(var Url, Empresa, Token, Pasta: String);
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
      Token := ExtrairValor(L)
    else if Pos('pasta_integracoes', L) = 1 then
      Pasta := ExtrairCaminho(L);
  end;
end;

{ Coleta as linhas do agente.toml existente que NAO sao as quatro chaves
  geridas por este instalador, devolvendo-as verbatim (comentarios e espacamento
  inclusive) para serem reanexadas ao arquivo novo.

  Motivo: CurStepChanged reescreve o arquivo do zero e SaveStringToFile com
  False sobrescreve. O agente 0.5.0 aceita max_consultas e max_exportacoes no
  mesmo arquivo, ajustaveis a mao quando o DB2 de um cliente esta sofrendo — e
  sao os unicos parametros do agente que afetam o sistema de caixa da loja. Sem
  esta funcao, essa valvula so funcionaria ate a proxima atualizacao.

  LIMITACAO CONHECIDA: a deteccao usa Pos(chave, L) = 1, a mesma convencao de
  LerConfigExistente. Uma chave futura que COMECE com 'servidor_url',
  'empresa_id', 'token' ou 'pasta_integracoes' — 'token_backup', por exemplo —
  seria considerada gerida por este instalador e portanto descartada. A convencao
  foi mantida por coerencia com o resto do arquivo; se surgir uma chave assim, os
  dois lugares precisam mudar juntos. }
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
    if Pos('pasta_integracoes', L) = 1 then Continue;
    { Linha original, nao a trimada: preserva indentacao e alinhamento. }
    Result := Result + Linhas[i] + #13#10;
  end;
end;

procedure ExecIcacls(const Parametros: String; var Falhas: String);
var
  ResultCode: Integer;
begin
  if not Exec(ExpandConstant('{sys}\icacls.exe'), Parametros, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    ResultCode := -1;
  if ResultCode <> 0 then
    Falhas := Falhas + #13#10 + '  ' + Parametros + '   (codigo ' + IntToStr(ResultCode) + ')';
end;

{ Fecha a pasta de config a Administrators e SYSTEM.

  NAO usa o parametro Permissions do [Dirs], e este e' o ponto principal: ele
  SOMA permissoes, nunca remove. A ACE users-modify que as versoes ate a 0.5.0
  gravaram continuaria la' depois de uma ATUALIZACAO, a pasta seguiria gravavel
  por qualquer conta interativa, e o instalador reportaria sucesso. Trocar so' a
  linha do [Dirs] conserta a instalacao NOVA e nao conserta nenhuma das sete
  maquinas que ja' tem o agente.

  SIDs e nao nomes de grupo: estas maquinas rodam Windows em portugues, onde os
  grupos se chamam Administradores e Usuarios. icacls por nome falharia em todas
  elas. S-1-5-32-544 = Administrators, S-1-5-18 = SYSTEM, S-1-5-32-545 = Users.

  Users fica com X (travessia) na pasta, e so' X. Sem isso, alcancar a subpasta
  logs dependeria da politica "Bypass traverse checking" — padrao no Windows,
  mas desligavel por GPO, e o sintoma seria o log da GUI sumir sem erro. X sem
  RD deixa passar por dentro sem listar a pasta nem ler o agente.toml, que e' o
  arquivo com o token. }
procedure AplicarPermissoes();
var
  Pasta, Logs, Falhas: String;
begin
  Pasta := ExpandConstant('{commonappdata}\Report Studio Agent');
  Logs := Pasta + '\logs';
  Falhas := '';

  ExecIcacls('"' + Pasta + '" /inheritance:r'
    + ' /remove:g *S-1-1-0 /remove:g *S-1-5-11'
    + ' /grant:r *S-1-5-32-544:(OI)(CI)F'
    + ' /grant:r *S-1-5-18:(OI)(CI)F'
    + ' /grant:r *S-1-5-32-545:X', Falhas);

  ExecIcacls('"' + Logs + '" /grant:r *S-1-5-32-545:(OI)(CI)M', Falhas);

  if Falhas <> '' then
    MsgBox('O agente foi instalado, mas as permissoes da pasta de configuracao nao'
      + ' ficaram como esperado.' + #13#10 + #13#10
      + 'O agente.toml guarda o token e pode continuar legivel por qualquer conta'
      + ' desta maquina. Verifique com:' + #13#10
      + '  icacls "' + Pasta + '"' + #13#10 + #13#10
      + 'Comandos que falharam:' + Falhas, mbError, MB_OK);
end;

procedure InitializeWizard;
var
  Url, Empresa, Token, Pasta: String;
begin
  PaginaConfig := CreateInputQueryPage(
    wpSelectTasks,
    'Configuracao do Agente',
    'Dados de conexao fornecidos pelo painel administrativo.',
    'Informe o ID da Empresa e o Token do Agente. Os dados sao gravados na configuracao e o servico inicia automaticamente.');
  PaginaConfig.Add('URL do Servidor:', False);
  PaginaConfig.Add('ID da Empresa:', False);
  PaginaConfig.Add('Token do Agente:', False);
  PaginaConfig.Add('Pasta de Integracoes (opcional):', False);

  // Prefill: usa o que ja estiver instalado; senao, a URL padrao.
  Url := '{#DefaultUrl}';
  Empresa := '';
  Token := '';
  Pasta := '';
  LerConfigExistente(Url, Empresa, Token, Pasta);
  if Trim(Url) = '' then
    Url := '{#DefaultUrl}';
  { O sugerido entra SO' em instalacao nova, detectada pela ausencia do
    agente.toml. Numa atualizacao o campo fica em branco quando o cliente nunca
    configurou integracao, e o valor dele aparece quando configurou.

    Pre-preencher na atualizacao gravaria a chave nas maquinas que nao usam
    integracao, e o log de subida passaria a dizer que a pasta esta configurada
    numa maquina onde ela nao existe — anulando justamente o resumo_integracoes
    que deveria responder "da' para entregar?" sem acesso remoto. }
  if not FileExists(ExpandConstant('{commonappdata}\Report Studio Agent\agente.toml')) then
    Pasta := '{#DefaultPastaIntegracoes}';

  PaginaConfig.Values[0] := Url;
  PaginaConfig.Values[1] := Empresa;
  PaginaConfig.Values[2] := Token;
  PaginaConfig.Values[3] := Pasta;
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
  Url, Empresa, Token, Pasta: String;
  ResultCode: Integer;
begin
  if CurStep <> ssPostInstall then
    Exit;

  Url := Trim(PaginaConfig.Values[0]);
  Empresa := Trim(PaginaConfig.Values[1]);
  Token := Trim(PaginaConfig.Values[2]);
  Pasta := Trim(PaginaConfig.Values[3]);

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
      'token        = "' + Token + '"' + #13#10;
    { Campo opcional: em branco, a chave nem aparece, e o agente trata como
      gravacao de arquivo indisponivel em vez de pasta invalida. }
    if Pasta <> '' then
      Conteudo := Conteudo +
        'pasta_integracoes = "' + EscaparToml(Pasta) + '"' + #13#10;
    Conteudo := Conteudo + Extras;
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

  AplicarPermissoes();
end;
