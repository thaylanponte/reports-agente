use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::gravador::{Destino, Pastas};

pub const SERVICE_NAME: &str = "ReportStudioAgent";
pub const SERVICE_DISPLAY: &str = "Report Studio Agent";

/// Nome do serviço da versão anterior. Mantido apenas para remoção na migração.
pub const SERVICE_NAME_ANTIGO: &str = "GuidettiAnalyticsAgent";

const PASTA_APP: &str = "Report Studio Agent";
const PASTA_APP_ANTIGA: &str = "Guidetti Agent";
const PASTA_LOGS: &str = "logs";

const TAMANHO_MAX_LOG: u64 = 5 * 1024 * 1024;
const ARQUIVOS_ROTACAO: u32 = 3;

/// Serializa escrita e rotação entre threads do mesmo processo.
static ESCRITA: Mutex<()> = Mutex::new(());

/// Resolvido uma vez, na primeira escrita. `None` = logging desabilitado.
static CAMINHO_LOG: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Token real do agente, registrado ao carregar/salvar a config, para que
/// `mascarar` consiga removê-lo por valor exato e não só por prefixo.
static TOKEN_AGENTE: RwLock<Option<String>> = RwLock::new(None);

#[derive(Clone, PartialEq)]
pub enum Status {
    Desconectado,
    Conectando,
    Conectado,
    Erro(String),
}

pub struct SharedState {
    pub logs: Vec<String>,
    pub status: Status,
}

impl SharedState {
    pub fn novo() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            logs: vec![],
            status: Status::Desconectado,
        }))
    }

    /// Funil único de log. Todo caminho do agente e da GUI passa por aqui.
    ///
    /// Mascara UMA vez, no ponto de entrada, para que os dois destinos — o `Vec`
    /// em memória que a GUI lê e o arquivo em disco — recebam texto já limpo.
    ///
    /// Nota de projeto (aceita, não é bug): o IO de arquivo roda aqui dentro, com
    /// o mutex do `SharedState` tomado, e no binário GUI a interface trava esse
    /// mesmo mutex 4x/s para repintar. É aceitável porque a escrita é curta e a
    /// rotação é rara. Se algum dia engasgar, a mitigação é manter o `File` aberto
    /// num `OnceLock<Mutex<File>>` em vez de abrir e fechar por linha.
    ///
    /// Limitação conhecida: o nível aqui é sempre INFO, então "Erro no stream"
    /// grava como INFO. Grepar o arquivo por ERRO não encontra erros do agente —
    /// para achá-los, grepe pelo texto da mensagem.
    pub fn log(&mut self, msg: &str) {
        let limpo = mascarar(msg);
        let hora = chrono::Local::now().format("%H:%M:%S");
        self.logs.push(format!("[{hora}] {limpo}"));
        if self.logs.len() > 500 {
            self.logs.drain(0..100);
        }
        escrever_linha("INFO", &limpo);
    }
}

// ─── Mascaramento de credencial ──────────────────────────────────────────────

/// Quanto se preserva do valor antes da máscara.
enum Mascara {
    /// Nada sobra. Usado para segredo puro.
    Total,
    /// Dois primeiros caracteres sobram, p.ex. `UID=db***`. Usado para o usuário:
    /// distinguir "usuário errado" de "senha errada" é diagnóstico legítimo e faz
    /// falta quando um cliente troca credencial.
    Parcial,
}

const CHAVES_SENSIVEIS: &[(&str, Mascara)] = &[
    ("password=", Mascara::Total),
    ("pwd=", Mascara::Total),
    ("uid=", Mascara::Parcial),
];

/// Remove credenciais de qualquer texto antes de ele sair do processo.
///
/// Aplicado em DOIS lugares, pelos dois motivos distintos:
///
/// 1. `SharedState::log` — o arquivo de log fica sob um diretório
///    `users-modify` (installer.iss:45), legível por qualquer usuário local
///    daquela máquina.
/// 2. `executor::erro` — o texto de erro do driver vira
///    `MensagemAgente::Erro { mensagem }`, atravessa a rede, é persistido na
///    coluna `erro_msg` de `jobs_execucao` num Postgres multi-tenant e aparece
///    no navegador do usuário. Esse caminho é o vazamento mais grave dos dois.
///
/// `DSN=`, `SERVER=` e `DATABASE=` ficam visíveis de propósito: não são segredo
/// e são justamente o que torna o log útil.
///
/// A função é idempotente — mascarar texto já mascarado devolve o mesmo texto.
pub fn mascarar(texto: &str) -> String {
    mascarar_token(&mascarar_chaves(texto))
}

/// Casa as chaves em qualquer posição, sem exigir que venham precedidas de `;`.
/// Isso pode mascarar de leve um `GUID=` (viraria `GUID=12***`), o que é
/// inofensivo. A escolha é deliberada: falso positivo custa ruído no log, falso
/// negativo custa uma senha do DB2 em disco e em banco.
fn mascarar_chaves(texto: &str) -> String {
    let bytes = texto.as_bytes();
    let mut saida = String::with_capacity(texto.len());
    let mut i = 0;

    while i < bytes.len() {
        let mut casou = false;

        for (chave, modo) in CHAVES_SENSIVEIS {
            let k = chave.len();
            if i + k <= bytes.len() && bytes[i..i + k].eq_ignore_ascii_case(chave.as_bytes()) {
                saida.push_str(&texto[i..i + k]);

                // O valor vai até o próximo ';' ou o fim da string.
                let resto = &texto[i + k..];
                let fim = resto.find(';').unwrap_or(resto.len());
                let valor = &resto[..fim];

                match modo {
                    Mascara::Total => saida.push_str("***"),
                    Mascara::Parcial => {
                        for c in valor.chars().take(2) {
                            saida.push(c);
                        }
                        saida.push_str("***");
                    }
                }

                i += k + fim;
                casou = true;
                break;
            }
        }

        if !casou {
            // Avança um caractere inteiro: fatiar por byte quebraria UTF-8.
            let largura = texto[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            saida.push_str(&texto[i..i + largura]);
            i += largura;
        }
    }

    saida
}

/// Duas camadas: o valor exato do token, quando já foi registrado pela config, e
/// o prefixo `gdt_` como rede de segurança para quando o token aparecer antes do
/// carregamento. O prefixo não é chute — é a convenção exibida em gui.rs:411.
fn mascarar_token(texto: &str) -> String {
    let mut saida = texto.to_string();

    if let Ok(guard) = TOKEN_AGENTE.read() {
        if let Some(token) = guard.as_ref() {
            // O piso de 8 evita que um token vazio ou minúsculo transforme o
            // replace numa substituição catastrófica.
            if token.len() >= 8 {
                saida = saida.replace(token.as_str(), "***");
            }
        }
    }

    mascarar_prefixo_gdt(&saida)
}

/// Variante do mascaramento para o caminho do hook de panic: degrada em vez de
/// bloquear.
///
/// `RwLock` não é reentrante. Se o panic ocorreu na mesma thread que já detinha
/// `TOKEN_AGENTE.write()` — dentro de `registrar_token` —, um `read()` aqui
/// travaria para sempre, ou panicaria, e o hook ficaria em silêncio no exato
/// cenário que ele existe para capturar. `unwrap_or_else(|e| e.into_inner())`
/// não cobre isso: reentrância é deadlock, não envenenamento.
///
/// `try_read` devolve `Err(WouldBlock)` nesse caso, e caímos para o
/// mascaramento por chaves literais e pelo prefixo `gdt_`, que não dependem de
/// lock nenhum. Perde-se só a remoção do token por valor exato.
fn mascarar_sem_bloquear(texto: &str) -> String {
    let por_chaves = mascarar_chaves(texto);

    let com_token = match TOKEN_AGENTE.try_read() {
        Ok(guard) => match guard.as_ref() {
            Some(token) if token.len() >= 8 => por_chaves.replace(token.as_str(), "***"),
            _ => por_chaves,
        },
        Err(_) => por_chaves,
    };

    mascarar_prefixo_gdt(&com_token)
}

fn mascarar_prefixo_gdt(texto: &str) -> String {
    const PREFIXO: &str = "gdt_";
    let bytes = texto.as_bytes();
    let mut saida = String::with_capacity(texto.len());
    let mut i = 0;

    while i < bytes.len() {
        let k = PREFIXO.len();
        if i + k <= bytes.len() && bytes[i..i + k].eq_ignore_ascii_case(PREFIXO.as_bytes()) {
            saida.push_str(&texto[i..i + k]);
            saida.push_str("***");
            i += k;
            // Consome o valor do token E os '*' de uma máscara anterior. Sem
            // incluir o '*' aqui, mascarar duas vezes produziria `gdt_******` —
            // a função deixaria de ser idempotente, e o funil mascara no
            // `SharedState::log` enquanto o `executor::erro` já mascarou antes.
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'*') {
                i += 1;
            }
        } else {
            let largura = texto[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            saida.push_str(&texto[i..i + largura]);
            i += largura;
        }
    }

    saida
}

fn registrar_token(token: &str) {
    if token.len() < 8 {
        return;
    }
    if let Ok(mut guard) = TOKEN_AGENTE.write() {
        *guard = Some(token.to_string());
    }
}

// ─── Log em arquivo ──────────────────────────────────────────────────────────

/// Resolve o caminho do log sem NUNCA panicar.
///
/// Deliberadamente não reusa `raiz_program_data()`, que tem `unwrap()` no
/// caminho de fallback: o hook de panic chama esta função, e um panic aqui
/// viraria double panic e abort — exatamente a falha que o hook existe para
/// registrar.
///
/// O nome sai do executável em vez de uma `init()` explícita, para que não haja
/// ordem de inicialização a respeitar e o `set_hook` possa ser literalmente a
/// primeira instrução de cada `main`.
fn caminho_log() -> Option<&'static PathBuf> {
    CAMINHO_LOG
        .get_or_init(|| {
            let raiz = std::env::var("ProgramData")
                .ok()
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                })?;

            let pasta = raiz.join(PASTA_APP).join(PASTA_LOGS);
            std::fs::create_dir_all(&pasta).ok()?;

            // Nome distinto por processo: o serviço roda como LocalSystem e a GUI
            // como usuário logado. Dois processos rotacionando o mesmo arquivo
            // corrompe a rotação.
            let stem = std::env::current_exe()
                .ok()
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
                .unwrap_or_default();

            let nome = if stem.contains("service") {
                "agente-service.log"
            } else {
                "agente-gui.log"
            };

            Some(pasta.join(nome))
        })
        .as_ref()
}

/// Rotação burra por tamanho. Teto de disco: 4 arquivos x 5 MB = 20 MB por
/// processo. Toda falha é ignorada — rotação não é motivo para perder log.
fn rotacionar_se_preciso(caminho: &PathBuf) {
    let Ok(meta) = std::fs::metadata(caminho) else {
        return;
    };
    if meta.len() < TAMANHO_MAX_LOG {
        return;
    }

    let base = caminho.to_string_lossy().to_string();

    // Descarta o mais antigo explicitamente, em vez de contar com o rename
    // sobrescrever.
    let _ = std::fs::remove_file(format!("{base}.{ARQUIVOS_ROTACAO}"));

    for i in (1..ARQUIVOS_ROTACAO).rev() {
        let _ = std::fs::rename(format!("{base}.{i}"), format!("{base}.{}", i + 1));
    }

    let _ = std::fs::rename(&base, format!("{base}.1"));
}

/// Anexa uma linha ao arquivo. Sem lock, sem rotação, sem nada que possa
/// panicar: nenhum `unwrap`, `expect`, indexação ou fatiamento. É o único ponto
/// que toca o disco, compartilhado pelos dois caminhos de escrita.
///
/// NÃO envolver em `BufWriter`. A escrita é direta no `File` de propósito: o
/// supervisor em service.rs chama `std::process::exit(1)` quando a thread do
/// agente morre, e buffer em userspace perderia exatamente a última linha — a
/// que explica o motivo da saída. O mesmo vale para o hook de panic.
fn anexar(caminho: &PathBuf, nivel: &str, ja_mascarado: &str) {
    let carimbo = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
    let linha = format!("{carimbo} [{nivel}] {ja_mascarado}\n");

    if let Ok(mut arquivo) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(caminho)
    {
        let _ = arquivo.write_all(linha.as_bytes());
    }
}

/// Caminho normal de escrita, bloqueante. Usado por `SharedState::log`.
///
/// Nenhuma falha de IO se propaga e nenhuma vira retry: falhou, segue em frente.
/// O lock é tolerante a envenenamento de propósito — sem isso, um panic durante
/// uma escrita desabilitaria o log para sempre, no exato momento em que ele mais
/// importa. Note que isso cobre envenenamento, NÃO reentrância: para essa, ver
/// `escrever_linha_sem_bloquear`.
fn escrever_linha(nivel: &str, ja_mascarado: &str) {
    let Some(caminho) = caminho_log() else {
        return;
    };

    let _guarda = ESCRITA.lock().unwrap_or_else(|e| e.into_inner());

    rotacionar_se_preciso(caminho);
    anexar(caminho, nivel, ja_mascarado);
}

/// Caminho de escrita do hook de panic: degrada em vez de bloquear.
///
/// `Mutex` não é reentrante. Se o panic ocorreu dentro de `escrever_linha`, com
/// `ESCRITA` já tomado por esta mesma thread, um `lock()` aqui travaria para
/// sempre e o hook não escreveria nada — silêncio total no cenário exato que ele
/// existe para capturar.
///
/// `try_lock` falha em vez de travar, e nesse caso escrevemos sem serialização:
/// uma linha entrelaçada é melhor que nenhuma linha.
///
/// A rotação fica de fora deste caminho de propósito. Sem a exclusão mútua, ela
/// poderia correr com outra thread renomeando o mesmo arquivo. Uma linha de
/// panic é curta, e a próxima escrita normal rotaciona.
fn escrever_linha_sem_bloquear(nivel: &str, ja_mascarado: &str) {
    let Some(caminho) = caminho_log() else {
        return;
    };

    // O guard, quando obtido, vive até o fim da função e serializa como de
    // costume. Quando não vem, seguimos sem ele.
    let _guarda = ESCRITA.try_lock().ok();

    anexar(caminho, nivel, ja_mascarado);
}

/// Log em arquivo para quem não tem `SharedState` à mão. Caminho bloqueante:
/// use apenas fora do hook de panic — hoje, `service_main` e a linha de subida
/// de cada binário.
#[allow(dead_code)]
pub fn log_arquivo(nivel: &str, msg: &str) {
    escrever_linha(nivel, &mascarar(msg));
}

/// Log em arquivo exclusivo do hook de panic. Nenhum lock bloqueante em todo o
/// caminho: `mascarar_sem_bloquear` usa `try_read` e `escrever_linha_sem_bloquear`
/// usa `try_lock`. Degradação em vez de deadlock.
fn log_arquivo_do_hook(nivel: &str, msg: &str) {
    escrever_linha_sem_bloquear(nivel, &mascarar_sem_bloquear(msg));
}

/// Instala o hook de panic. Deve ser a PRIMEIRA instrução de cada `main`, antes
/// de `carregar_config()` — shared.rs tem `unwrap()` no caminho de resolução do
/// ProgramData, e um panic ali escaparia sem registro se o hook viesse depois.
///
/// O hook NÃO toca no `SharedState`: se o panic aconteceu com aquele mutex
/// tomado — dentro de `s.log()`, ou num dos `.lock().unwrap()` da GUI —, chamar
/// de volta ali daria deadlock ou double panic. Escreve direto no arquivo.
///
/// Pelo mesmo motivo, usa `log_arquivo_do_hook` e não `log_arquivo`: nenhum lock
/// do caminho do hook pode ser bloqueante, porque um panic pode ter ocorrido com
/// `ESCRITA` ou `TOKEN_AGENTE` já tomados por esta mesma thread, e nem `Mutex`
/// nem `RwLock` são reentrantes.
pub fn instalar_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<payload não textual>".to_string()
        };

        let local = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<local desconhecido>".to_string());

        // `capture()` já respeita RUST_BACKTRACE sozinho: devolve `Disabled`
        // quando a variável não está ativa. Não é preciso checar o env à mão.
        let backtrace = std::backtrace::Backtrace::capture();
        let trilha = if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            format!("\n{backtrace}")
        } else {
            String::new()
        };

        let msg = format!("PANIC em {local}: {payload}{trilha}");

        log_arquivo_do_hook("PANIC", &msg);

        // Preserva o comportamento do hook padrão para os subcomandos de console
        // (install/status/...), que dependem de ver o panic no terminal.
        eprintln!("{msg}");
    }));
}

// ─── Limites de concorrência ─────────────────────────────────────────────────

/// Faixas separadas de concorrência. O teto único de 1 nunca protegeu o ERP:
/// `DescreverConsulta` não passa por semáforo nenhum e cada uma abre conexão DB2
/// real com `prepare`. Separar as faixas faz uma exportação longa parar de
/// recusar consultas, e uma consulta travada parar de bloquear exportações.
///
/// O padrão de consultas é 3, e não 1, porque `ExecutarConsulta` mistura cargas
/// de duração muito diferente na mesma faixa. A query de 10 minutos da Análise
/// de Giro (analise_giro/servico.rs:204) é uma `ExecutarConsulta`, e os
/// dropdowns da mesma tela também são — seções, grupos, subgrupos e
/// representantes (analise_giro/servico.rs:118, 136, 154, 171) e os seletores de
/// empresa e fornecedor (servico_seletor.rs:117-126). Com a faixa em 1, um único
/// usuário gerando o PDF do Giro derruba os dropdowns de TODOS os usuários com
/// "agente ocupado" durante os 10 minutos inteiros.
///
/// Isto é paliativo. O fix estrutural é o Giro virar job assíncrono no servidor
/// (Fase 2.2), tirando a query longa do caminho de `ExecutarConsulta`. Enquanto
/// isso não acontece, 3 permits dão folga para a navegação sobreviver a um Giro
/// em andamento sem abrir mão do teto.
const MAX_CONSULTAS_PADRAO: usize = 3;
const MAX_EXPORTACOES_PADRAO: usize = 1;

/// Os tetos não são arbitrários: são o ponto em que a proteção do ERP deixa de
/// existir e o agente passa a ser o problema em vez da defesa. Estes dois
/// números são os únicos parâmetros do agente que afetam o sistema de caixa da
/// loja, não o Report Studio — por isso são configuráveis, e por isso têm teto.
///
/// O piso é 1 porque um agente com zero permits não executaria nada e pareceria
/// travado, sem nenhuma mensagem que explicasse o motivo.
const MAX_CONSULTAS_MIN: usize = 1;
const MAX_CONSULTAS_MAX: usize = 10;
const MAX_EXPORTACOES_MIN: usize = 1;
const MAX_EXPORTACOES_MAX: usize = 3;

/// Limites efetivos entregues ao agente.
///
/// Struct, e não dois `usize` soltos na assinatura de `AgentHandle::iniciar`,
/// porque dois parâmetros `usize` adjacentes trocam de lugar em silêncio: o
/// build passa e o agente roda com 1 faixa de consulta e 3 de exportação, uma
/// configuração plausível e errada que nenhum teste pegaria.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitesConcorrencia {
    pub consultas: usize,
    pub exportacoes: usize,
}

/// Aplica o padrão quando ausente e o clamp quando fora da faixa.
///
/// Valor fora do intervalo é AJUSTADO, nunca rejeitado: recusar a config
/// deixaria o agente sem subir por causa de um número digitado errado. O zero
/// cai no mesmo mecanismo, sem ramo especial — `0.clamp(1, n) == 1`.
fn resolver_limite(bruto: Option<usize>, padrao: usize, min: usize, max: usize) -> usize {
    match bruto {
        None => padrao,
        Some(v) => v.clamp(min, max),
    }
}

/// Descreve um limite para o log de subida: valor efetivo, origem e ajuste.
fn descrever_limite(
    nome: &str,
    bruto: Option<usize>,
    efetivo: usize,
    min: usize,
    max: usize,
) -> String {
    match bruto {
        None => format!("{nome}={efetivo} (padrão)"),
        Some(v) if v == efetivo => format!("{nome}={efetivo} (agente.toml)"),
        Some(v) if v > max => {
            format!("{nome}={efetivo} (agente.toml, ajustado de {v} — máximo é {max})")
        }
        Some(v) => format!("{nome}={efetivo} (agente.toml, ajustado de {v} — mínimo é {min})"),
    }
}

#[derive(Clone, Default)]
pub struct AgentConfig {
    pub url: String,
    pub empresa_id: String,
    pub token: String,
    /// Valores CRUS, como lidos do arquivo — não os efetivos.
    ///
    /// Guardar o cru é o que faz um ajuste feito à mão sobreviver a um
    /// salvamento pela GUI. Normalizar aqui significaria que o arquivo seria
    /// corrigido apenas quando alguém por acaso clicasse em "Salvar", e não nas
    /// outras vezes — não determinístico, pior que qualquer das duas opções
    /// consistentes.
    ///
    /// Risco residual aceito: o arquivo pode dizer `max_consultas = 50` enquanto
    /// o agente opera com 10, e quem ler o toml sem olhar o log se engana. É
    /// aceitável porque `resumo_limites` é gravado a CADA subida, não uma única
    /// vez — o log sempre tem a resposta atual.
    pub max_consultas: Option<usize>,
    pub max_exportacoes: Option<usize>,
    /// Pasta do Client da eSales. O nome genérico é herança da 0.6.0, quando só
    /// havia esse destino; renomear quebraria o agente.toml das máquinas atuais.
    pub pasta_integracoes: Option<String>,
    /// Pasta `documents\out` do Client da NeoGrid.
    pub pasta_neogrid: Option<String>,
}

fn caminho_preenchido(bruto: &Option<String>) -> Option<PathBuf> {
    bruto
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

fn resumo_pasta(chave: &str, pasta: Option<PathBuf>) -> String {
    match pasta {
        None => format!("{chave} ausente — gravação indisponível"),
        Some(p) if p.is_dir() => format!("{chave} = {} (pasta existe)", p.display()),
        Some(p) => format!(
            "{chave} = {} — PASTA NÃO ENCONTRADA, a gravação vai falhar",
            p.display()
        ),
    }
}

impl AgentConfig {
    pub fn token_completo(&self) -> String {
        if self.empresa_id.is_empty() {
            self.token.clone()
        } else {
            format!("{}:{}", self.empresa_id, self.token)
        }
    }

    pub fn valida(&self) -> bool {
        !self.url.is_empty() && !self.token.is_empty()
    }

    pub fn pastas(&self) -> Pastas {
        Pastas {
            esales: caminho_preenchido(&self.pasta_integracoes),
            neogrid: caminho_preenchido(&self.pasta_neogrid),
        }
    }

    /// Linha única para o log de subida, no mesmo espírito de `resumo_limites`:
    /// sem a pasta, `GravarArquivo` falha para SEMPRE naquela máquina, e hoje
    /// isso só apareceria quando alguém tentasse enviar.
    ///
    /// Ao contrário de `resumo_limites`, esta NÃO é pura: toca o disco. É
    /// deliberado — a pergunta que o suporte faz é "dá para entregar?", e a
    /// chave estar preenchida não responde isso. Uma pasta configurada que não
    /// existe é o modo de falha mais provável, porque os Clients da eSales e da
    /// NeoGrid são instalados por terceiros e podem não ter criado a pasta ainda.
    ///
    /// "PASTA NÃO ENCONTRADA" em caixa alta é para ser greppável: `SharedState`
    /// grava tudo como INFO (ver a limitação em `SharedState::log`), então não
    /// há nível que destaque este caso.
    pub fn resumo_integracoes(&self) -> String {
        let pastas = self.pastas();
        format!(
            "Integrações: {}; {}",
            resumo_pasta(Destino::Esales.chave_config(), pastas.esales),
            resumo_pasta(Destino::Neogrid.chave_config(), pastas.neogrid)
        )
    }

    /// Limites efetivos. Pura: não loga e não toca em nenhum global.
    pub fn limites(&self) -> LimitesConcorrencia {
        LimitesConcorrencia {
            consultas: resolver_limite(
                self.max_consultas,
                MAX_CONSULTAS_PADRAO,
                MAX_CONSULTAS_MIN,
                MAX_CONSULTAS_MAX,
            ),
            exportacoes: resolver_limite(
                self.max_exportacoes,
                MAX_EXPORTACOES_PADRAO,
                MAX_EXPORTACOES_MIN,
                MAX_EXPORTACOES_MAX,
            ),
        }
    }

    /// Linha única para o log de subida. Sem isto não há como saber, olhando o
    /// log de uma máquina, se alguém editou o agente.toml — que é justamente o
    /// cenário em que o log vai ser lido.
    pub fn resumo_limites(&self) -> String {
        let efetivos = self.limites();
        format!(
            "Limites: {}, {}",
            descrever_limite(
                "max_consultas",
                self.max_consultas,
                efetivos.consultas,
                MAX_CONSULTAS_MIN,
                MAX_CONSULTAS_MAX
            ),
            descrever_limite(
                "max_exportacoes",
                self.max_exportacoes,
                efetivos.exportacoes,
                MAX_EXPORTACOES_MIN,
                MAX_EXPORTACOES_MAX
            ),
        )
    }
}

fn raiz_program_data() -> PathBuf {
    std::env::var("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf()
        })
}

fn pasta_config() -> PathBuf {
    let pasta = raiz_program_data().join(PASTA_APP);

    if !pasta.exists() {
        let _ = std::fs::create_dir_all(&pasta);
    }

    pasta
}

pub fn caminho_config() -> PathBuf {
    pasta_config().join("agente.toml")
}

/// Migra o agente.toml da pasta da versão antiga ("Guidetti Agent") para a nova,
/// caso a config nova ainda não exista. Evita que clientes percam a configuração
/// ao atualizar para o nome novo.
fn migrar_config_antiga() {
    let novo = caminho_config(); // garante a criação da pasta nova
    if novo.exists() {
        return;
    }

    let antigo = raiz_program_data()
        .join(PASTA_APP_ANTIGA)
        .join("agente.toml");

    if antigo.exists() {
        let _ = std::fs::copy(&antigo, &novo);
    }
}

/// Interpreta o conteúdo do agente.toml. Pura: sem IO, sem global — é o que
/// torna o comportamento testável com fixtures literais, incluindo o caso
/// crítico do arquivo de produção que não tem as chaves de limite.
fn parsear_config(conteudo: &str) -> AgentConfig {
    #[derive(serde::Deserialize, Default)]
    struct Cfg {
        servidor_url: Option<String>,
        empresa_id: Option<String>,
        token: Option<String>,
        /// Ausentes em todo agente.toml hoje em produção. `default` garante que
        /// a falta não vire erro de desserialização.
        #[serde(default)]
        max_consultas: Option<usize>,
        #[serde(default)]
        max_exportacoes: Option<usize>,
        #[serde(default)]
        pasta_integracoes: Option<String>,
        #[serde(default)]
        pasta_neogrid: Option<String>,
    }

    let cfg: Cfg = toml::from_str(conteudo).unwrap_or_default();

    let url = cfg.servidor_url.unwrap_or_default();
    let token_raw = cfg.token.unwrap_or_default();
    let max_consultas = cfg.max_consultas;
    let max_exportacoes = cfg.max_exportacoes;
    let pasta_integracoes = cfg.pasta_integracoes;
    let pasta_neogrid = cfg.pasta_neogrid;

    // Formato novo: empresa_id como campo separado
    if let Some(empresa_id) = cfg.empresa_id {
        return AgentConfig {
            url,
            empresa_id,
            token: token_raw,
            max_consultas,
            max_exportacoes,
            pasta_integracoes,
            pasta_neogrid,
        };
    }

    // Retrocompatibilidade: formato antigo com empresa_id:token junto
    let (empresa_id, token) = if let Some(pos) = token_raw.find(':') {
        (token_raw[..pos].to_string(), token_raw[pos + 1..].to_string())
    } else {
        (String::new(), token_raw)
    };

    AgentConfig {
        url,
        empresa_id,
        token,
        max_consultas,
        max_exportacoes,
        pasta_integracoes,
        pasta_neogrid,
    }
}

pub fn carregar_config() -> AgentConfig {
    migrar_config_antiga();

    let caminho = caminho_config();
    let conteudo = match std::fs::read_to_string(&caminho) {
        Ok(c) => c,
        Err(_) => return AgentConfig::default(),
    };

    let config = parsear_config(&conteudo);
    registrar_token(&config.token);
    config
}

pub fn salvar_config(config: &AgentConfig) {
    registrar_token(&config.token);

    let mut conteudo = format!("servidor_url = \"{}\"\n", config.url);

    if !config.empresa_id.is_empty() {
        conteudo.push_str(&format!("empresa_id   = \"{}\"\n", config.empresa_id));
    }

    conteudo.push_str(&format!("token        = \"{}\"\n", config.token));

    // Reescreve os limites SÓ quando presentes, com o valor cru. Sem isto, um
    // ajuste feito à mão no arquivo seria apagado no primeiro clique em "Salvar"
    // pela GUI — que é o cenário mais provável, já que quem edita o limite
    // dificilmente é quem mexe na tela depois.
    //
    // Nota: este arquivo é montado à mão, não serializado por serde. Um
    // `skip_serializing_if` não teria efeito aqui.
    if let Some(v) = config.max_consultas {
        conteudo.push_str(&format!("max_consultas   = {v}\n"));
    }
    if let Some(v) = config.max_exportacoes {
        conteudo.push_str(&format!("max_exportacoes = {v}\n"));
    }
    for (chave, valor) in [
        ("pasta_integracoes", &config.pasta_integracoes),
        ("pasta_neogrid", &config.pasta_neogrid),
    ] {
        if let Some(p) = valor {
            let escapado = p.replace('\\', "\\\\").replace('"', "\\\"");
            conteudo.push_str(&format!("{chave} = \"{escapado}\"\n"));
        }
    }

    let _ = std::fs::write(caminho_config(), conteudo);
}

#[cfg(test)]
mod tests_limites {
    use super::*;

    // ─── Clamp ───────────────────────────────────────────────────────────────

    #[test]
    fn dentro_da_faixa_e_respeitado() {
        assert_eq!(resolver_limite(Some(5), 3, 1, 10), 5);
        assert_eq!(resolver_limite(Some(2), 1, 1, 3), 2);
    }

    #[test]
    fn acima_do_teto_e_ajustado_para_o_teto() {
        assert_eq!(resolver_limite(Some(50), 3, 1, 10), 10);
        assert_eq!(resolver_limite(Some(9), 1, 1, 3), 3);
    }

    #[test]
    fn abaixo_do_piso_e_ajustado_para_o_piso() {
        // Só alcançável com min > 1; o clamp é o mesmo mecanismo do zero.
        assert_eq!(resolver_limite(Some(1), 3, 2, 10), 2);
    }

    #[test]
    fn zero_vira_o_piso() {
        // Agente com zero permits não executaria nada e pareceria travado.
        assert_eq!(resolver_limite(Some(0), 3, 1, 10), 1);
        assert_eq!(resolver_limite(Some(0), 1, 1, 3), 1);
    }

    #[test]
    fn ausente_usa_o_padrao() {
        assert_eq!(resolver_limite(None, 3, 1, 10), 3);
        assert_eq!(resolver_limite(None, 1, 1, 3), 1);
    }

    // ─── Parse: compatibilidade com o que está em produção ───────────────────

    /// CRÍTICO. Formato exato do agente.toml em campo hoje: sem as chaves novas
    /// e com o token no formato legado `empresa_id:token`. Se este teste
    /// quebrar, toda máquina instalada para de carregar a configuração.
    #[test]
    fn toml_de_producao_sem_as_chaves_novas_carrega() {
        let conteudo = "\
servidor_url = \"wss://reports.exemplo.com.br/ws/agente\"
token        = \"11111111-2222-3333-4444-555555555555:gdt_tokensinteticoparateste\"
";

        let cfg = parsear_config(conteudo);

        assert_eq!(cfg.url, "wss://reports.exemplo.com.br/ws/agente");
        assert_eq!(cfg.empresa_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(cfg.token, "gdt_tokensinteticoparateste");
        assert_eq!(cfg.max_consultas, None);
        assert_eq!(cfg.max_exportacoes, None);

        // E os limites caem nos padrões, sem erro.
        assert_eq!(
            cfg.limites(),
            LimitesConcorrencia {
                consultas: MAX_CONSULTAS_PADRAO,
                exportacoes: MAX_EXPORTACOES_PADRAO,
            }
        );
    }

    /// Formato novo, com empresa_id separado, também sem as chaves de limite.
    #[test]
    fn toml_formato_novo_sem_as_chaves_novas_carrega() {
        let conteudo = "\
servidor_url = \"wss://reports.exemplo.com.br/ws/agente\"
empresa_id   = \"11111111-2222-3333-4444-555555555555\"
token        = \"gdt_tokensinteticoparateste\"
";

        let cfg = parsear_config(conteudo);

        assert_eq!(cfg.empresa_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(cfg.token, "gdt_tokensinteticoparateste");
        assert_eq!(cfg.max_consultas, None);
        assert_eq!(cfg.limites().consultas, MAX_CONSULTAS_PADRAO);
    }

    #[test]
    fn toml_com_os_limites_le_os_valores() {
        let conteudo = "\
servidor_url = \"wss://reports.exemplo.com.br/ws/agente\"
empresa_id   = \"11111111-2222-3333-4444-555555555555\"
token        = \"gdt_tokensinteticoparateste\"
max_consultas   = 2
max_exportacoes = 2
";

        let cfg = parsear_config(conteudo);

        assert_eq!(cfg.max_consultas, Some(2));
        assert_eq!(cfg.max_exportacoes, Some(2));
        assert_eq!(
            cfg.limites(),
            LimitesConcorrencia {
                consultas: 2,
                exportacoes: 2
            }
        );
    }

    #[test]
    fn valor_absurdo_e_clampado_mas_o_cru_e_preservado() {
        let conteudo = "\
servidor_url = \"wss://x/ws\"
token        = \"gdt_tokensinteticoparateste\"
max_consultas = 50
";

        let cfg = parsear_config(conteudo);

        // O cru sobrevive — é ele que volta para o arquivo num salvamento.
        assert_eq!(cfg.max_consultas, Some(50));
        // O efetivo é o teto.
        assert_eq!(cfg.limites().consultas, MAX_CONSULTAS_MAX);
    }

    // ─── Resumo para o log ───────────────────────────────────────────────────

    #[test]
    fn resumo_distingue_padrao_de_arquivo_de_ajuste() {
        let padrao = AgentConfig::default();
        assert_eq!(
            padrao.resumo_limites(),
            "Limites: max_consultas=3 (padrão), max_exportacoes=1 (padrão)"
        );

        let do_arquivo = AgentConfig {
            max_consultas: Some(2),
            ..Default::default()
        };
        assert_eq!(
            do_arquivo.resumo_limites(),
            "Limites: max_consultas=2 (agente.toml), max_exportacoes=1 (padrão)"
        );

        let ajustado = AgentConfig {
            max_consultas: Some(50),
            ..Default::default()
        };
        assert_eq!(
            ajustado.resumo_limites(),
            "Limites: max_consultas=10 (agente.toml, ajustado de 50 — máximo é 10), \
             max_exportacoes=1 (padrão)"
        );
    }

    #[test]
    fn resumo_distingue_os_tres_estados_da_pasta() {
        assert!(AgentConfig::default()
            .resumo_integracoes()
            .contains("ausente"));

        let em_branco = AgentConfig {
            pasta_integracoes: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(em_branco.resumo_integracoes().contains("ausente"));

        let existente = AgentConfig {
            pasta_integracoes: Some(std::env::temp_dir().to_string_lossy().to_string()),
            ..Default::default()
        };
        let resumo = existente.resumo_integracoes();
        assert!(resumo.contains("(pasta existe)"));
        assert!(!resumo.contains("PASTA NÃO ENCONTRADA"));

        let inexistente = AgentConfig {
            pasta_integracoes: Some(
                std::env::temp_dir()
                    .join("rs-agente-pasta-que-nao-existe")
                    .to_string_lossy()
                    .to_string(),
            ),
            ..Default::default()
        };
        let resumo = inexistente.resumo_integracoes();
        assert!(resumo.contains("PASTA NÃO ENCONTRADA"));
        assert!(resumo.contains("rs-agente-pasta-que-nao-existe"));
    }

    #[test]
    fn resumo_mostra_os_dois_destinos() {
        let so_neogrid = AgentConfig {
            pasta_neogrid: Some(std::env::temp_dir().to_string_lossy().to_string()),
            ..Default::default()
        };
        let resumo = so_neogrid.resumo_integracoes();
        assert!(resumo.contains("pasta_integracoes ausente"), "{resumo}");
        assert!(resumo.contains("pasta_neogrid = "), "{resumo}");
        assert!(resumo.contains("(pasta existe)"), "{resumo}");
    }

    #[test]
    fn pastas_le_os_dois_destinos_do_toml() {
        let cfg = parsear_config(
            "servidor_url = \"wss://x\"\n\
             empresa_id   = \"e\"\n\
             token        = \"t\"\n\
             pasta_integracoes = 'C:\\eSales\\Envio'\n\
             pasta_neogrid = \"C:\\\\ngclient-6.0.1\\\\NeoGridClient\\\\documents\\\\out\"\n",
        );
        let pastas = cfg.pastas();
        assert_eq!(pastas.esales, Some(PathBuf::from(r"C:\eSales\Envio")));
        assert_eq!(
            pastas.neogrid,
            Some(PathBuf::from(r"C:\ngclient-6.0.1\NeoGridClient\documents\out"))
        );

        let sem_neogrid = parsear_config("servidor_url = \"wss://x\"\ntoken = \"t\"\n");
        assert_eq!(sem_neogrid.pastas().neogrid, None);
    }
}

#[cfg(test)]
mod tests {
    use super::mascarar;

    #[test]
    fn mascara_valor_no_meio_da_string() {
        let entrada = "DSN=DB2;UID=db2admin;PWD=segredo123;DATABASE=PROD";
        let saida = mascarar(entrada);
        assert_eq!(saida, "DSN=DB2;UID=db***;PWD=***;DATABASE=PROD");
        assert!(!saida.contains("segredo123"));
        assert!(!saida.contains("db2admin"));
        // DSN e DATABASE seguem visíveis: não são segredo e são o que
        // torna o log útil.
        assert!(saida.contains("DSN=DB2"));
        assert!(saida.contains("DATABASE=PROD"));
    }

    #[test]
    fn mascara_valor_no_fim_sem_ponto_e_virgula() {
        let saida = mascarar("Falha na conexão: DSN=DB2;PWD=ultimo_valor");
        assert_eq!(saida, "Falha na conexão: DSN=DB2;PWD=***");
        assert!(!saida.contains("ultimo_valor"));
    }

    #[test]
    fn mascara_em_caixa_mista() {
        let saida = mascarar("Pwd=Abc;uid=Fulano;PaSsWoRd=XyZ");
        assert_eq!(saida, "Pwd=***;uid=Fu***;PaSsWoRd=***");
        assert!(!saida.contains("Abc"));
        assert!(!saida.contains("XyZ"));
        assert!(!saida.contains("Fulano"));
    }

    #[test]
    fn mascara_multiplas_ocorrencias() {
        let saida = mascarar("PWD=um;outro=x;PWD=dois;PWD=tres");
        assert_eq!(saida, "PWD=***;outro=x;PWD=***;PWD=***");
        assert!(!saida.contains("um;"));
        assert!(!saida.contains("dois"));
        assert!(!saida.contains("tres"));
    }

    #[test]
    fn nao_altera_string_sem_nada_a_mascarar() {
        let entrada = "Consulta abc-123: 4000 linhas em 812ms";
        assert_eq!(mascarar(entrada), entrada);
    }

    #[test]
    fn e_idempotente() {
        let uma = mascarar("UID=db2admin;PWD=segredo;token gdt_1a2b3c4d");
        let duas = mascarar(&uma);
        assert_eq!(uma, duas);
    }

    #[test]
    fn mascara_token_por_prefixo() {
        let saida = mascarar("Autenticando com gdt_36eb3df54a64491b8b67e5f83fbe9c6a agora");
        assert_eq!(saida, "Autenticando com gdt_*** agora");
        assert!(!saida.contains("36eb3df5"));
    }

    #[test]
    fn preserva_utf8_multibyte() {
        // Fatiar por byte em vez de por caractere quebraria a string aqui.
        let saida = mascarar("Exportação não concluída — PWD=x");
        assert_eq!(saida, "Exportação não concluída — PWD=***");
    }
}
