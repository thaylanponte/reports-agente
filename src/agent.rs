use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::executor::ExecutorConsulta;
use crate::gravador::{self, Destino, Pastas};
use crate::shared::{LimitesConcorrencia, SharedState, Status};

// Os limites de concorrência (max_consultas / max_exportacoes) NÃO moram aqui:
// são configuráveis pelo agente.toml e vivem em shared.rs, junto de
// `AgentConfig`. Chegam a `loop_agente` via `LimitesConcorrencia`.
//
// Os valores abaixo, ao contrário, NÃO são configuráveis, e a distinção é
// deliberada: concorrência é ajuste de carga, timeout é contrato.
//
// `DEADLINE_OCIOSIDADE_SECS` deriva do Ping de 30s do servidor
// (agent_hub.rs:372-383) — mexer nele sem olhar o servidor quebra a detecção de
// queda. `TIMEOUT_EXPORTACAO_SECS` participa de uma invariante de escalonamento
// entre três camadas:
//
//     agente (1800s) < hub (2100s) < servico_exportacao (2160s)
//
// Só o agente consegue PARAR a exportação, via a flag de aborto; as outras duas
// só removem o pendente. Expor esses números no agente.toml convidaria alguém a
// inverter a ordem sem saber que a invariante existe.

/// Teto do watchdog de exportação. Atenção: governa **apenas** exportação.
/// `ExecutarConsulta` não tem teto nenhum no agente — o único limite é o
/// `AGENT_QUERY_TIMEOUT_SECS` do servidor (agent_hub.rs:228). Depois que o
/// servidor desiste, o agente segue executando e segurando o permit da faixa de
/// consulta, e não há como interrompê-lo até existir `MensagemServidor::Cancelar`.
const TIMEOUT_EXPORTACAO_SECS: u64 = 1800;

const KEEPALIVE_INTERVALO_SECS: u64 = 25;
const RECONNECT_BASE_SECS: u64 = 5;
const RECONNECT_MAX_SECS: u64 = 60;
const ODBC_RETRY_SECS: u64 = 10;

/// Teto para o handshake completo: TCP + TLS + upgrade WebSocket.
const CONNECT_TIMEOUT_SECS: u64 = 20;

/// Silêncio máximo tolerado do servidor. O servidor envia Ping a cada 30s para
/// todo agente registrado (agent_hub.rs:372-383), incondicionalmente, sem checar
/// job pendente — logo 90s são 3 pings perdidos. Vale desde o connect, cobrindo
/// também a janela pré-autenticação, em que não há tráfego nenhum do servidor.
const DEADLINE_OCIOSIDADE_SECS: u64 = 90;

/// Espera máxima na fila do semáforo antes de recusar o job.
const TIMEOUT_AQUISICAO_SECS: u64 = 90;

/// Prazo para o runtime encerrar antes de a thread do agente desistir dele.
const SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// `destino_neogrid` é o que libera, no servidor, o envio de `GravarArquivoDestino`
/// com destino NeoGrid. Uma capacidade por destino, e não uma genérica: o
/// servidor precisa saber que ESTE agente conhece ESTE destino, porque um
/// agente que não conhece recusa a mensagem só depois do timeout.
const CAPACIDADES: &[&str] = &["gravar_arquivo", "celulas_texto", "destino_neogrid"];

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "tipo", rename_all = "snake_case")]
enum MensagemServidor {
    ExecutarConsulta {
        id: String,
        sql: String,
        connection_string: String,
        limite: usize,
    },
    DescreverConsulta {
        id: String,
        sql: String,
        connection_string: String,
    },
    IniciarExportacao {
        id: String,
        sql: String,
        connection_string: String,
        chunk_size: usize,
    },
    /// Contrato da 0.6.0: sempre eSales. Continua existindo porque o servidor
    /// ainda manda a Lactalis por aqui.
    GravarArquivo {
        id: String,
        nome: String,
        conteudo_b64: String,
    },
    /// Tipo novo, e não um campo `destino` opcional no `GravarArquivo`: a 0.6.0
    /// ignora campo desconhecido, e um pedido da NeoGrid lido por ela gravaria
    /// na pasta da eSales, por cima do arquivo de mesmo nome da Lactalis. Com
    /// tipo novo, agente antigo descarta a mensagem em vez de errar o destino.
    ///
    /// `destino` é texto e não `Destino`: chave desconhecida vira `Erro`
    /// correlacionado em vez de falha de parse, que o servidor só veria como
    /// timeout.
    GravarArquivoDestino {
        id: String,
        destino: String,
        nome: String,
        conteudo_b64: String,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "tipo", rename_all = "snake_case")]
enum MensagemAgente {
    Autenticar {
        token: String,
        /// Versão do agente. O servidor ignora campos desconhecidos
        /// (agent_hub.rs:38-39 não tem `deny_unknown_fields`), então isto é
        /// retrocompatível e não precisa esperar rollout do servidor.
        versao: String,
        #[serde(default)]
        capacidades: Vec<String>,
    },
    Resultado {
        id: String,
        colunas: Vec<String>,
        linhas: Vec<Vec<serde_json::Value>>,
        total_linhas: usize,
        tempo_execucao_ms: u64,
    },
    ResultadoDescricao {
        id: String,
        colunas: Vec<String>,
    },
    ChunkExportacao {
        id: String,
        colunas: Vec<String>,
        linhas: Vec<Vec<serde_json::Value>>,
        chunk_idx: usize,
        fim: bool,
    },
    ArquivoGravado {
        id: String,
        caminho: String,
        sha256: String,
        bytes: u64,
    },
    Erro {
        id: String,
        mensagem: String,
    },
    Pong,
}

pub struct AgentHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AgentHandle {
    pub fn iniciar(
        url: String,
        token: String,
        limites: LimitesConcorrencia,
        pastas: Pastas,
        state: Arc<Mutex<SharedState>>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let thread = std::thread::spawn(move || {
            // Falhar de forma limpa é melhor que panicar. O hook de panic da 1B
            // registraria o panic, mas aqui dá para reportar o estado e sair.
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    log(
                        &state,
                        &format!("Falha ao criar o runtime tokio: {e}. O agente não vai subir."),
                    );
                    set_status(&state, Status::Erro("Runtime indisponível".into()));
                    return;
                }
            };

            rt.block_on(async {
                tokio::select! {
                    _ = loop_agente(url, token, limites, pastas, state) => {}
                    _ = shutdown_rx => {}
                }
            });

            // `Runtime::drop` (tokio 1.49, runtime.rs:494-511) espera TODAS as
            // spawn_blocking terminarem, e tarefa blocking é incancelável — é
            // isso que trava o serviço em "Parando" quando há consulta ODBC
            // pendurada, e obriga o Gerenciador de Tarefas. `shutdown_timeout`
            // (runtime.rs:446) desiste no prazo e devolve a thread; as threads
            // blocking sobreviventes morrem junto com o processo.
            rt.shutdown_timeout(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS));
        });

        Self {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    pub fn parar(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Consulta, sem bloquear, se a thread do agente terminou.
    ///
    /// `is_finished()` também devolve true quando a thread termina normalmente
    /// após um shutdown pedido — mas aqui isso não gera ambiguidade, e a razão é
    /// estrutural: `loop_agente` é um laço infinito que nunca retorna, então a
    /// thread só pode terminar por (a) o oneshot de shutdown disparar, o que só
    /// acontece dentro de `parar()`, ou (b) panic / falha ao criar o runtime.
    ///
    /// Quem chama deve consultar isto apenas enquanto nenhum shutdown foi
    /// solicitado. O laço de supervisão em service.rs garante isso por
    /// construção, checando só no braço de Timeout.
    ///
    /// Devolve `false` quando o handle já foi consumido: reportar morte sem
    /// evidência seria pior que não reportar.
    ///
    /// Só o binário do serviço consome isto; no da GUI a supervisão não existe,
    /// porque a janela mostra o estado direto.
    #[allow(dead_code)]
    pub fn morreu(&self) -> bool {
        self.thread.as_ref().map_or(false, |t| t.is_finished())
    }
}

fn log(state: &Arc<Mutex<SharedState>>, msg: &str) {
    if let Ok(mut s) = state.lock() {
        s.log(msg);
    }
}

fn set_status(state: &Arc<Mutex<SharedState>>, status: Status) {
    if let Ok(mut s) = state.lock() {
        s.status = status;
    }
}

fn enviar(tx: &mpsc::UnboundedSender<String>, msg: &MensagemAgente) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = tx.send(json);
    }
}

fn gravar_arquivo(
    destino: Result<Destino, String>,
    id: String,
    nome: String,
    conteudo_b64: String,
    pastas: &Pastas,
    state: &Arc<Mutex<SharedState>>,
    ws_tx: &mpsc::UnboundedSender<String>,
) {
    log(state, &format!("Gravando arquivo {nome} ({id})..."));

    let pastas = pastas.clone();
    let state = state.clone();
    let ws_tx = ws_tx.clone();

    tokio::task::spawn_blocking(move || {
        let gravacao = destino.and_then(|d| {
            gravador::gravar(d, pastas.de(d), &id, &nome, &conteudo_b64)
        });

        let resposta = match gravacao {
            Ok(g) => {
                log(
                    &state,
                    &format!("Arquivo {id} gravado em {} ({} bytes).", g.caminho, g.bytes),
                );
                MensagemAgente::ArquivoGravado {
                    id,
                    caminho: g.caminho,
                    sha256: g.sha256,
                    bytes: g.bytes,
                }
            }
            Err(e) => {
                log(&state, &format!("Erro ao gravar arquivo {id}: {e}"));
                MensagemAgente::Erro { id, mensagem: e }
            }
        };

        enviar(&ws_tx, &resposta);
    });
}

async fn loop_agente(
    url: String,
    token: String,
    limites: LimitesConcorrencia,
    pastas: Pastas,
    state: Arc<Mutex<SharedState>>,
) {
    // Init do ODBC com retry: se falhar (ex.: subsistema ainda não pronto no boot),
    // tenta novamente em vez de encerrar o agente silenciosamente.
    let executor = loop {
        match ExecutorConsulta::novo() {
            Ok(e) => break Arc::new(e),
            Err(e) => {
                log(
                    &state,
                    &format!("Erro ao inicializar ODBC: {e}. Nova tentativa em {ODBC_RETRY_SECS}s..."),
                );
                set_status(&state, Status::Erro("ODBC indisponível".into()));
                tokio::time::sleep(Duration::from_secs(ODBC_RETRY_SECS)).await;
            }
        }
    };

    let mut tentativas_reconexao: u32 = 0;

    loop {
        // Semáforos por conexão, e não globais: uma chamada ODBC travada é
        // incancelável, então o permit dela nunca volta. Recriando por sessão, o
        // permit preso morre junto com a conexão antiga em vez de esterilizar o
        // agente para sempre.
        //
        // Troca aceita: os tetos deixam de ser garantia global. Uma thread ODBC
        // travada da sessão antiga coexiste com a nova, e sob falha persistente
        // do DB2 isso acumula threads bloqueadas (teto do pool do tokio: 512).
        // Fecha na Fase 3, com watchdog de progresso.
        let semaforo_consulta = Arc::new(tokio::sync::Semaphore::new(limites.consultas));
        let semaforo_exportacao = Arc::new(tokio::sync::Semaphore::new(limites.exportacoes));

        log(&state, &format!("Conectando em {url}..."));
        set_status(&state, Status::Conectando);

        let tentativa = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
            connect_async(&url),
        )
        .await;

        match tentativa {
            Ok(Ok((ws, _))) => {
                tentativas_reconexao = 0;

                log(&state, "WebSocket conectado. Autenticando...");
                let (mut sink, mut stream) = ws.split();

                let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();

                tokio::spawn(async move {
                    while let Some(json) = ws_rx.recv().await {
                        if sink.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                });

                enviar(
                    &ws_tx,
                    &MensagemAgente::Autenticar {
                        token: token.clone(),
                        versao: env!("CARGO_PKG_VERSION").to_string(),
                        capacidades: CAPACIDADES.iter().map(|c| c.to_string()).collect(),
                    },
                );
                log(&state, "Autenticado. Aguardando comandos...");
                set_status(&state, Status::Conectado);

                let keepalive_tx = ws_tx.clone();
                let keepalive_state = state.clone();
                let keepalive = tokio::spawn(async move {
                    let start = tokio::time::Instant::now() + Duration::from_secs(KEEPALIVE_INTERVALO_SECS);
                    let mut interval = tokio::time::interval_at(start, Duration::from_secs(KEEPALIVE_INTERVALO_SECS));
                    loop {
                        interval.tick().await;
                        enviar(&keepalive_tx, &MensagemAgente::Pong);
                        log(&keepalive_state, "Keepalive enviado.");
                    }
                });

                // Separa "o servidor nunca falou" (autenticação não pegou) de "o
                // servidor parou de falar" (queda silenciosa). Sem ACK de
                // autenticação, é o único jeito de distinguir os dois no log.
                let mut recebeu_algo = false;

                loop {
                    let proximo = tokio::time::timeout(
                        Duration::from_secs(DEADLINE_OCIOSIDADE_SECS),
                        stream.next(),
                    )
                    .await;

                    let resultado = match proximo {
                        Ok(Some(r)) => r,
                        Ok(None) => {
                            log(&state, "Stream encerrado pelo servidor.");
                            break;
                        }
                        Err(_) => {
                            if recebeu_algo {
                                log(
                                    &state,
                                    &format!(
                                        "Sem tráfego do servidor há {DEADLINE_OCIOSIDADE_SECS}s, reconectando..."
                                    ),
                                );
                            } else {
                                log(
                                    &state,
                                    &format!(
                                        "Sem tráfego do servidor há {DEADLINE_OCIOSIDADE_SECS}s desde o connect \
                                         (autenticação não confirmada), reconectando..."
                                    ),
                                );
                            }
                            break;
                        }
                    };

                    recebeu_algo = true;

                    let msg = match resultado {
                        Ok(m) => m,
                        Err(e) => {
                            log(&state, &format!("Erro no stream: {e}"));
                            break;
                        }
                    };

                    let texto = match msg {
                        Message::Text(t) => t.to_string(),
                        Message::Close(_) => {
                            log(&state, "Servidor fechou a conexão.");
                            break;
                        }
                        Message::Ping(_) => {
                            enviar(&ws_tx, &MensagemAgente::Pong);
                            continue;
                        }
                        _ => continue,
                    };

                    let mensagem: MensagemServidor = match serde_json::from_str(&texto) {
                        Ok(m) => m,
                        Err(e) => {
                            log(&state, &format!("Mensagem inválida: {e}"));
                            continue;
                        }
                    };

                    match mensagem {
                        MensagemServidor::ExecutarConsulta {
                            id,
                            sql,
                            connection_string,
                            limite,
                        } => {
                            log(&state, &format!("Executando consulta {id}..."));

                            let executor_clone = executor.clone();
                            let state_clone = state.clone();
                            let ws_tx_clone = ws_tx.clone();
                            let semaforo_clone = semaforo_consulta.clone();

                            // O acquire vai para DENTRO da task: se ficasse aqui,
                            // uma consulta longa cegaria o laço de leitura e o
                            // deadline de ociosidade derrubaria a própria conexão.
                            tokio::spawn(async move {
                                let permit = match tokio::time::timeout(
                                    Duration::from_secs(TIMEOUT_AQUISICAO_SECS),
                                    semaforo_clone.acquire_owned(),
                                )
                                .await
                                {
                                    Ok(Ok(p)) => p,
                                    // Semáforo fechado: a sessão está encerrando.
                                    Ok(Err(_)) => return,
                                    Err(_) => {
                                        log(
                                            &state_clone,
                                            &format!(
                                                "Consulta {id} recusada: agente ocupado há mais de {TIMEOUT_AQUISICAO_SECS}s."
                                            ),
                                        );
                                        enviar(
                                            &ws_tx_clone,
                                            &MensagemAgente::Erro {
                                                id,
                                                mensagem: "agente ocupado, tente novamente".into(),
                                            },
                                        );
                                        return;
                                    }
                                };

                                tokio::task::spawn_blocking(move || {
                                    let _permit = permit;

                                    let resposta = match executor_clone.executar(&connection_string, &sql, limite) {
                                        Ok(r) => {
                                            log(
                                                &state_clone,
                                                &format!("Consulta {id}: {} linhas em {}ms", r.total_linhas, r.tempo_execucao_ms),
                                            );
                                            MensagemAgente::Resultado {
                                                id,
                                                colunas: r.colunas,
                                                linhas: r.linhas,
                                                total_linhas: r.total_linhas,
                                                tempo_execucao_ms: r.tempo_execucao_ms,
                                            }
                                        }
                                        Err(e) => {
                                            log(&state_clone, &format!("Erro na consulta {id}: {e}"));
                                            MensagemAgente::Erro { id, mensagem: e }
                                        }
                                    };

                                    enviar(&ws_tx_clone, &resposta);
                                });
                            });
                        }

                        MensagemServidor::DescreverConsulta {
                            id,
                            sql,
                            connection_string,
                        } => {
                            log(&state, &format!("Descrevendo consulta {id}..."));

                            let executor_clone = executor.clone();
                            let state_clone = state.clone();
                            let ws_tx_clone = ws_tx.clone();

                            tokio::task::spawn_blocking(move || {
                                let resposta = match executor_clone.descrever(&connection_string, &sql) {
                                    Ok(colunas) => {
                                        log(&state_clone, &format!("Descricao {id}: {} colunas", colunas.len()));
                                        MensagemAgente::ResultadoDescricao { id, colunas }
                                    }
                                    Err(e) => {
                                        log(&state_clone, &format!("Erro na descricao {id}: {e}"));
                                        MensagemAgente::Erro { id, mensagem: e }
                                    }
                                };

                                enviar(&ws_tx_clone, &resposta);
                            });
                        }

                        MensagemServidor::IniciarExportacao {
                            id,
                            sql,
                            connection_string,
                            chunk_size,
                        } => {
                            log(&state, &format!("Exportando {id} (chunk_size={chunk_size})..."));

                            let executor_clone = executor.clone();
                            let ws_tx_clone = ws_tx.clone();
                            let state_clone = state.clone();
                            let semaforo_clone = semaforo_exportacao.clone();
                            let ws_tx_timeout = ws_tx.clone();
                            let state_timeout = state.clone();

                            tokio::spawn(async move {
                                // Flag de aborto da exportação. Fica nomeada aqui, no
                                // topo da task, e não enterrada dentro do select!,
                                // porque a Fase 2 (MensagemServidor::Cancelar { id })
                                // vai reaproveitar exatamente esta flag com outro
                                // gatilho, alcançável por id.
                                let abortar = Arc::new(AtomicBool::new(false));
                                let abortar_worker = abortar.clone();

                                let permit = match tokio::time::timeout(
                                    Duration::from_secs(TIMEOUT_AQUISICAO_SECS),
                                    semaforo_clone.acquire_owned(),
                                )
                                .await
                                {
                                    Ok(Ok(p)) => p,
                                    // Semáforo fechado: a sessão está encerrando.
                                    Ok(Err(_)) => return,
                                    Err(_) => {
                                        log(
                                            &state_clone,
                                            &format!(
                                                "Exportação {id} recusada: agente ocupado há mais de {TIMEOUT_AQUISICAO_SECS}s."
                                            ),
                                        );
                                        enviar(
                                            &ws_tx_clone,
                                            &MensagemAgente::Erro {
                                                id,
                                                mensagem: "agente ocupado, tente novamente".into(),
                                            },
                                        );
                                        return;
                                    }
                                };

                                let id_task = id.clone();

                                let handle = tokio::task::spawn_blocking(move || {
                                    let _permit = permit;

                                    let id_export = id_task.clone();

                                    let resultado = executor_clone.exportar_streaming(
                                        &connection_string,
                                        &sql,
                                        chunk_size,
                                        |colunas, linhas, chunk_idx, fim| {
                                            enviar(
                                                &ws_tx_clone,
                                                &MensagemAgente::ChunkExportacao {
                                                    id: id_export.clone(),
                                                    colunas,
                                                    linhas,
                                                    chunk_idx,
                                                    fim,
                                                },
                                            );

                                            if fim {
                                                log(
                                                    &state_clone,
                                                    &format!("Exportação {} concluída (chunk {chunk_idx}).", id_export),
                                                );
                                            }

                                            // Devolver false faz executor.rs:139-143
                                            // parar com `return Ok(())` — logo o
                                            // `if let Err(e)` abaixo não dispara e
                                            // nenhum erro duplicado é enviado.
                                            //
                                            // Limitação: só funciona se a exportação
                                            // estiver progredindo. Travamento dentro
                                            // do fetch() (executor.rs:128-130) nunca
                                            // chama esta callback e a flag nunca é
                                            // lida — esse caso é Fase 3/4.
                                            !abortar_worker.load(Ordering::Relaxed)
                                        },
                                    );

                                    if let Err(e) = resultado {
                                        log(&state_clone, &format!("Erro na exportação {id_task}: {e}"));
                                        enviar(&ws_tx_clone, &MensagemAgente::Erro { id: id_task, mensagem: e });
                                    }

                                    // `exportar_streaming` devolve Ok(()) quando a
                                    // callback interrompe, então o `if let Err` acima
                                    // não dispara e o aborto não deixaria rastro
                                    // nenhum. Sem esta linha não há como saber se a
                                    // flag surtiu efeito ou se a exportação terminou
                                    // sozinha antes de vê-la.
                                    if abortar_worker.load(Ordering::Relaxed) {
                                        log(
                                            &state_clone,
                                            &format!("Exportação {id_export} interrompida pela flag de aborto."),
                                        );
                                    }
                                });

                                tokio::select! {
                                    _ = handle => {}
                                    _ = tokio::time::sleep(Duration::from_secs(TIMEOUT_EXPORTACAO_SECS)) => {
                                        // Setar a flag ANTES de enviar o erro. O handle é
                                        // dropado quando o select! termina, o que NÃO
                                        // cancela a spawn_blocking — correto aqui, porque
                                        // ela precisa continuar até o próximo chunk para
                                        // enxergar a flag e liberar o permit.
                                        abortar.store(true, Ordering::Relaxed);

                                        log(
                                            &state_timeout,
                                            &format!("Timeout: exportação {id} excedeu {TIMEOUT_EXPORTACAO_SECS}s. Aborto solicitado."),
                                        );

                                        // Id real, não String::new(): no servidor,
                                        // pendentes_export só sai do HashMap com chunk
                                        // final, erro correlacionado ou desconexão. Com id
                                        // vazio, erro_exportacao("") não casa nada
                                        // (agent_hub.rs:366) e a entrada vaza permanente.
                                        //
                                        // Esta linha conserta o vazamento no SERVIDOR. O
                                        // desperdício no agente quem corrige é a flag
                                        // acima — e só no próximo limite de chunk.
                                        enviar(&ws_tx_timeout, &MensagemAgente::Erro {
                                            id: id.clone(),
                                            mensagem: "Timeout: exportação excedeu o tempo limite no agente".into(),
                                        });
                                    }
                                }
                            });
                        }

                        MensagemServidor::GravarArquivo {
                            id,
                            nome,
                            conteudo_b64,
                        } => {
                            gravar_arquivo(
                                Ok(Destino::Esales),
                                id,
                                nome,
                                conteudo_b64,
                                &pastas,
                                &state,
                                &ws_tx,
                            );
                        }

                        MensagemServidor::GravarArquivoDestino {
                            id,
                            destino,
                            nome,
                            conteudo_b64,
                        } => {
                            gravar_arquivo(
                                Destino::da_chave(&destino),
                                id,
                                nome,
                                conteudo_b64,
                                &pastas,
                                &state,
                                &ws_tx,
                            );
                        }

                        MensagemServidor::Ping => {
                            enviar(&ws_tx, &MensagemAgente::Pong);
                        }
                    }
                }

                keepalive.abort();
                set_status(&state, Status::Desconectado);
                log(&state, "Conexão encerrada.");
            }
            Ok(Err(e)) => {
                log(&state, &format!("Falha ao conectar: {e}"));
                set_status(&state, Status::Erro(format!("{e}")));
            }
            Err(_) => {
                log(
                    &state,
                    &format!("Timeout de {CONNECT_TIMEOUT_SECS}s no handshake. Reconectando..."),
                );
                set_status(&state, Status::Erro("Timeout no handshake".into()));
            }
        }

        tentativas_reconexao += 1;
        let base = (RECONNECT_BASE_SECS * tentativas_reconexao as u64).min(RECONNECT_MAX_SECS);
        // Jitter: espalha as reconexões entre 50% e 100% da base. Sem isso, todos
        // os agentes voltam no mesmo instante depois de um reload do proxy.
        // Não usar SystemTime::subsec_nanos() como fonte: no Windows o relógio
        // resolve em unidades de 100ns (FILETIME), e o resto da divisão degenera
        // justamente nos degraus iniciais (base=5 daria sempre 2s).
        let espera = rand::thread_rng().gen_range(base / 2..=base);
        log(&state, &format!("Reconectando em {espera}s..."));
        tokio::time::sleep(Duration::from_secs(espera)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autenticar_leva_versao_e_capacidades() {
        let json = serde_json::to_value(MensagemAgente::Autenticar {
            token: "gdt_x".to_string(),
            versao: "0.6.0".to_string(),
            capacidades: CAPACIDADES.iter().map(|c| c.to_string()).collect(),
        })
        .unwrap();

        assert_eq!(json["tipo"], "autenticar");
        assert_eq!(json["token"], "gdt_x");
        assert_eq!(json["versao"], "0.6.0");
        assert_eq!(json["capacidades"][0], "gravar_arquivo");
        assert_eq!(json["capacidades"][1], "celulas_texto");
        assert_eq!(json["capacidades"][2], "destino_neogrid");
    }

    #[test]
    fn autenticar_de_agente_antigo_sem_capacidades_desserializa() {
        let msg: MensagemAgente =
            serde_json::from_str(r#"{"tipo":"autenticar","token":"t","versao":"0.5.0"}"#).unwrap();

        match msg {
            MensagemAgente::Autenticar { capacidades, .. } => assert!(capacidades.is_empty()),
            _ => panic!("variante errada"),
        }
    }

    #[test]
    fn gravar_arquivo_desserializa_o_contrato() {
        let msg: MensagemServidor = serde_json::from_str(
            r#"{"tipo":"gravar_arquivo","id":"i","nome":"ACC.txt","conteudo_b64":"QQ=="}"#,
        )
        .unwrap();

        assert!(matches!(msg, MensagemServidor::GravarArquivo { .. }));
    }

    #[test]
    fn gravar_arquivo_destino_desserializa_o_contrato() {
        let msg: MensagemServidor = serde_json::from_str(
            r#"{"tipo":"gravar_arquivo_destino","id":"i","destino":"neogrid","nome":"ACC.txt","conteudo_b64":"QQ=="}"#,
        )
        .unwrap();

        match msg {
            MensagemServidor::GravarArquivoDestino { destino, .. } => {
                assert_eq!(Destino::da_chave(&destino).unwrap(), Destino::Neogrid)
            }
            _ => panic!("variante errada"),
        }
    }

    #[test]
    fn gravar_arquivo_destino_sem_destino_nao_vira_gravar_arquivo() {
        let msg = serde_json::from_str::<MensagemServidor>(
            r#"{"tipo":"gravar_arquivo_destino","id":"i","nome":"ACC.txt","conteudo_b64":"QQ=="}"#,
        );
        assert!(msg.is_err());
    }

    #[test]
    fn arquivo_gravado_serializa_o_contrato() {
        let json = serde_json::to_value(MensagemAgente::ArquivoGravado {
            id: "i".to_string(),
            caminho: r"C:\eSales\Envio\ACC.txt".to_string(),
            sha256: "abc".to_string(),
            bytes: 7,
        })
        .unwrap();

        assert_eq!(json["tipo"], "arquivo_gravado");
        assert_eq!(json["id"], "i");
        assert_eq!(json["caminho"], r"C:\eSales\Envio\ACC.txt");
        assert_eq!(json["sha256"], "abc");
        assert_eq!(json["bytes"], 7);
    }
}
