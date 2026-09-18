use std::ffi::OsString;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use windows_service::define_windows_service;
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
    ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::agent::AgentHandle;
use crate::shared::{
    carregar_config, log_arquivo, SharedState, Status, SERVICE_DISPLAY, SERVICE_NAME,
};

define_windows_service!(ffi_service_main, service_main);

/// Período após o qual o SCM zera o contador de falhas. 900s e não 86400s: com
/// um dia inteiro, o contador só zera após 24h sem nenhuma falha, então
/// incidentes esparsos vão escalando e você acaba caindo sempre no atraso de
/// 60s. Com 15 minutos o contador zera rápido e quase sempre pega a primeira
/// ação, que é o restart em 10s.
const RESET_FALHAS_SECS: u64 = 900;

/// Atrasos das três ações de restart do SCM.
const ATRASOS_RESTART_SECS: [u64; 3] = [10, 30, 60];

/// Intervalo do laço de supervisão da thread do agente.
const INTERVALO_SUPERVISAO_SECS: u64 = 5;

/// `wait_hint` do StopPending. 30s e não 10s: o `shutdown_timeout` do runtime já
/// consome até 10s sozinho, mais o join e o overhead, o que deixava o orçamento
/// no limite do que o SCM aceita antes de considerar o serviço travado.
const WAIT_HINT_STOP_SECS: u64 = 30;

/// Guardado para que `service_main` consiga reportar `Stopped` de último recurso
/// quando `run_service` sai por erro. `ServiceStatusHandle` é `Send + Sync` por
/// declaração explícita do crate (service_control_handler.rs:45-46), então o
/// static é sólido.
static STATUS_HANDLE: OnceLock<ServiceStatusHandle> = OnceLock::new();

pub fn run() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

pub fn instalar() {
    let exe_path = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("Erro ao obter caminho do executável: {e}");
        std::process::exit(1);
    });

    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CREATE_SERVICE,
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Erro ao acessar gerenciador de serviços: {e}");
            eprintln!("Execute como Administrador.");
            std::process::exit(1);
        }
    };

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    match manager.create_service(
        &service_info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG | ServiceAccess::START,
    ) {
        Ok(service) => {
            println!("Serviço '{SERVICE_NAME}' instalado com sucesso.");
            configurar_recuperacao(&service, &service_info);
        }
        Err(e) => {
            eprintln!("Erro ao instalar serviço: {e}");
            std::process::exit(1);
        }
    }
}

/// A política de recuperação do serviço, num lugar só.
///
/// Três restarts com atrasos crescentes e contador zerando em 15 minutos. Sem o
/// `failureflag` (aplicado à parte, em `aplicar_politica_recuperacao`) nada disto
/// dispara quando o serviço termina reportando `SERVICE_STOPPED` — que é
/// justamente o caminho do supervisor em `run_service`.
fn politica_falhas() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(RESET_FALHAS_SECS)),
        reboot_msg: None,
        command: None,
        actions: Some(
            ATRASOS_RESTART_SECS
                .iter()
                .map(|s| ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(*s),
                })
                .collect(),
        ),
    }
}

/// Parte compartilhada entre o install e a autocura na subida: tudo que não
/// depende de `start_type`.
///
/// Devolve a lista de falhas; vazia significa que tudo foi aplicado. Cada
/// chamada é idempotente, então dá para rodar em toda subida sem checar antes.
///
/// Usa a API tipada do `windows-service` 0.8.0, não `sc.exe`: o crate expõe
/// `set_failure_actions_on_non_crash_failures` (service.rs:1647),
/// `update_failure_actions` (:1773) e `set_delayed_auto_start` (:1831).
fn aplicar_politica_recuperacao(service: &Service) -> Vec<String> {
    let mut falhas = Vec::new();

    // O failureflag é o que faz o SCM tratar "parou reportando Stopped com
    // código != 0" como falha. Sem ele, todo o desenho de watchdog e supervisão
    // não recupera nada.
    if let Err(e) = service.set_failure_actions_on_non_crash_failures(true) {
        falhas.push(format!("failureflag (SERVICE_FAILURE_ACTIONS_FLAG): {e}"));
    }

    if let Err(e) = service.update_failure_actions(politica_falhas()) {
        falhas.push(format!("ações de restart e reset: {e}"));
    }

    // Espera a rede e o cliente DB2 subirem antes de o agente tentar conectar.
    if let Err(e) = service.set_delayed_auto_start(true) {
        falhas.push(format!("início automático atrasado: {e}"));
    }

    falhas
}

/// Configura a recuperação no momento do install.
///
/// Antes esta função chamava `sc.exe` e estava errada em dois pontos que
/// custaram caro: punha `reset= 86400` e nunca setava o `failureflag`. Como o
/// installer.iss roda `agente-service.exe install` a cada atualização, toda
/// instalação desfazia em silêncio a configuração aplicada à mão nas máquinas.
/// A autocura em `autocurar_configuracao` foi desenhada como contorno disto —
/// mas contorno não substitui consertar a origem.
///
/// Este é também o ÚNICO lugar que consegue agir sobre `start_type`: roda
/// elevado e antes de o serviço estar rodando. A autocura na subida não alcança
/// isso, porque com `start_type` em Manual o serviço não sobe no boot e a
/// autocura nunca executa.
///
/// Ao contrário da subida, aqui o operador está olhando a tela: falha é
/// reportada com destaque, não engolida.
fn configurar_recuperacao(service: &Service, service_info: &ServiceInfo) {
    let mut falhas = aplicar_politica_recuperacao(service);

    // `create_service` já cria com AutoStart, então isto normalmente é um
    // no-op. Fica como garantia para o caso de a função ser chamada sobre um
    // serviço preexistente.
    match service.query_config() {
        Ok(config) if config.start_type != ServiceStartType::AutoStart => {
            if let Err(e) = service.change_config(service_info) {
                falhas.push(format!(
                    "start_type (está em {:?}, deveria ser AutoStart): {e}",
                    config.start_type
                ));
            }
        }
        Ok(_) => {}
        Err(e) => falhas.push(format!("leitura da configuração do serviço: {e}")),
    }

    if falhas.is_empty() {
        println!("Recuperação automática configurada (restart 10s/30s/60s, reset 15min).");
        return;
    }

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════════╗");
    eprintln!("║  ATENÇÃO: o serviço foi instalado, mas a recuperação automática  ║");
    eprintln!("║  NÃO ficou configurada. Se o agente cair, ninguém vai levantar.  ║");
    eprintln!("╚══════════════════════════════════════════════════════════════════╝");
    for falha in &falhas {
        eprintln!("  - {falha}");
    }
    eprintln!();
    eprintln!("  Rode como Administrador para corrigir à mão:");
    eprintln!("    sc.exe failureflag {SERVICE_NAME} 1");
    eprintln!(
        "    sc.exe failure {SERVICE_NAME} reset= {RESET_FALHAS_SECS} \
         actions= restart/10000/restart/30000/restart/60000"
    );
    eprintln!("    sc.exe config {SERVICE_NAME} start= delayed-auto");
    eprintln!();

    log_arquivo(
        "ERRO",
        &format!("Install: recuperação automática não configurada: {}", falhas.join("; ")),
    );
}

/// Verifica e corrige a configuração de recuperação a cada subida do serviço.
///
/// Existe porque o installer.iss desfaz essa configuração a cada atualização —
/// ver `configurar_recuperacao`. Mesmo com aquela função corrigida, esta segue
/// valendo: cobre máquinas que já receberam instaladores antigos e qualquer
/// alteração manual posterior.
///
/// NÃO mexe em `depend=`. `sc config depend=` substitui a lista inteira, e se
/// algum serviço listado estiver desabilitado o alvo passa a falhar ao iniciar
/// com erro 1068, inclusive no boot. Esse é aplicado à mão, uma vez, por máquina.
///
/// NÃO mexe em `start_type`, e não por escolha estética: se estiver em Manual, o
/// serviço não sobe no boot, logo esta função nunca roda para consertar. É
/// autocura que não teria como funcionar — só registra o aviso.
///
/// Toda falha aqui é NÃO FATAL: loga e segue. O agente tem que subir mesmo sem
/// conseguir se reconfigurar.
fn autocurar_configuracao() {
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        Ok(m) => m,
        Err(e) => {
            log_arquivo("AVISO", &format!("Autocura: sem acesso ao SCM: {e}"));
            return;
        }
    };

    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(s) => s,
        Err(e) => {
            log_arquivo("AVISO", &format!("Autocura: não foi possível abrir o serviço: {e}"));
            return;
        }
    };

    // Diagnóstico antes de corrigir: se o failureflag estiver desligado ou o
    // reset vier em 86400, é a assinatura de um instalador antigo tendo passado
    // por aqui. Registrar isso é o que permite saber, depois, por que uma
    // máquina não se recuperou.
    if let Ok(false) = service.get_failure_actions_on_non_crash_failures() {
        log_arquivo(
            "AVISO",
            "Autocura: failureflag estava DESLIGADO — recuperação do SCM não dispararia. Corrigindo.",
        );
    }
    if let Ok(atual) = service.get_failure_actions() {
        if atual.reset_period != ServiceFailureResetPeriod::After(Duration::from_secs(RESET_FALHAS_SECS))
        {
            log_arquivo(
                "AVISO",
                &format!("Autocura: reset de falhas estava em {:?}. Corrigindo.", atual.reset_period),
            );
        }
    }

    let falhas = aplicar_politica_recuperacao(&service);
    if !falhas.is_empty() {
        log_arquivo(
            "AVISO",
            &format!("Autocura: não foi possível aplicar {}", falhas.join("; ")),
        );
    }

    if let Ok(config) = service.query_config() {
        if config.start_type != ServiceStartType::AutoStart {
            log_arquivo(
                "AVISO",
                &format!(
                    "start_type está em {:?}, não em AutoStart — o serviço NÃO sobe sozinho no \
                     boot e o início atrasado é ignorado. Corrija com: \
                     sc.exe config {SERVICE_NAME} start= delayed-auto",
                    config.start_type
                ),
            );
        }
    }
}

pub fn desinstalar() {
    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Erro ao acessar gerenciador de serviços: {e}");
            std::process::exit(1);
        }
    };

    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Serviço não encontrado: {e}");
            std::process::exit(1);
        }
    };

    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    match service.delete() {
        Ok(_) => println!("Serviço '{SERVICE_NAME}' removido com sucesso."),
        Err(e) => {
            eprintln!("Erro ao remover serviço: {e}");
            std::process::exit(1);
        }
    }
}

pub fn iniciar() {
    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Erro ao acessar gerenciador de serviços: {e}");
            std::process::exit(1);
        }
    };

    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::START) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Serviço não encontrado: {e}");
            std::process::exit(1);
        }
    };

    match service.start(&[] as &[&str]) {
        Ok(_) => println!("Serviço iniciado."),
        Err(e) => {
            eprintln!("Erro ao iniciar: {e}");
            std::process::exit(1);
        }
    }
}

pub fn parar() {
    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Erro ao acessar gerenciador de serviços: {e}");
            std::process::exit(1);
        }
    };

    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::STOP) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Serviço não encontrado: {e}");
            std::process::exit(1);
        }
    };

    match service.stop() {
        Ok(_) => println!("Serviço parado."),
        Err(e) => {
            eprintln!("Erro ao parar: {e}");
            std::process::exit(1);
        }
    }
}

/// Para o serviço (se estiver rodando), aguarda ele parar e inicia novamente.
/// Usado pela GUI para aplicar uma nova configuração com um único prompt de UAC.
pub fn reiniciar() {
    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Erro ao acessar gerenciador de serviços: {e}");
            std::process::exit(1);
        }
    };

    if let Ok(service) =
        manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
    {
        if let Ok(status) = service.query_status() {
            if status.current_state != ServiceState::Stopped {
                let _ = service.stop();
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(500));
                    if let Ok(s) = service.query_status() {
                        if s.current_state == ServiceState::Stopped {
                            break;
                        }
                    }
                }
            }
        }
    }

    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::START) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Serviço não encontrado: {e}");
            std::process::exit(1);
        }
    };

    match service.start(&[] as &[&str]) {
        Ok(_) => println!("Serviço reiniciado."),
        Err(e) => {
            eprintln!("Erro ao iniciar: {e}");
            std::process::exit(1);
        }
    }
}

pub fn mostrar_status() {
    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    ) {
        Ok(m) => m,
        Err(_) => {
            println!("nao_instalado");
            return;
        }
    };

    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(_) => {
            println!("nao_instalado");
            return;
        }
    };

    match service.query_status() {
        Ok(status) => {
            let estado = match status.current_state {
                ServiceState::Stopped => "parado",
                ServiceState::Running => "rodando",
                ServiceState::StartPending => "iniciando",
                ServiceState::StopPending => "parando",
                _ => "desconhecido",
            };
            println!("{estado}");
        }
        Err(_) => println!("erro"),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        log_arquivo("ERRO", &format!("run_service falhou: {e}"));

        // Último recurso: sem isto o SCM registra terminação inesperada. Se o
        // próprio register() falhou, não existe handle e só resta o log.
        if let Some(handle) = STATUS_HANDLE.get() {
            let _ = handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(1),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            });
        }
    }
}

fn run_service() -> windows_service::Result<()> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control_event| match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

    let _ = STATUS_HANDLE.set(status_handle);

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;

    let state = Arc::new(Mutex::new(SharedState {
        logs: vec![],
        status: Status::Desconectado,
    }));

    let config = carregar_config();

    if !config.valida() {
        // A mensagem precisa nomear CONFIG explicitamente. Com o failureflag
        // agora em 1 nas máquinas, um Stopped com código diferente de zero
        // dispara recuperação do SCM — e como config malformada é falha
        // permanente, isso vira laço de restart a cada 60s. Quem for ler o log
        // precisa distinguir isso de um problema de rede em dois segundos, senão
        // vai caçar firewall enquanto o agente.toml está vazio.
        log_arquivo(
            "ERRO",
            "Config inválida em agente.toml (servidor_url ou token ausente). \
             Isto é problema de CONFIGURAÇÃO, não de rede. O serviço vai parar e \
             o SCM vai retentar em laço até a config ser corrigida.",
        );

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(2),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;
        return Ok(());
    }

    let url = config.url.clone();
    let token_completo = config.token_completo();
    let limites = config.limites();
    let pasta_integracoes = config.pasta_integracoes();
    let handle = AgentHandle::iniciar(url, token_completo, limites, pasta_integracoes, state);

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Depois de reportar Running, não antes: evita gastar o orçamento do
    // wait_hint de subida com round-trips ao SCM, e evita o risco de um serviço
    // conversar com o SCM sobre si mesmo antes de ter reportado Running.
    autocurar_configuracao();

    // Supervisão da thread do agente. Antes isto era um `recv()` sem prazo, que
    // nunca observava a morte da filha: se ela morresse, o SCM seguia
    // reportando Running indefinidamente com nada rodando.
    //
    // A checagem de morte acontece SÓ no braço de Timeout, ou seja, apenas
    // enquanto nenhum shutdown foi solicitado. É isso que distingue "a thread
    // morreu" de "a thread terminou porque pedimos".
    loop {
        match shutdown_rx.recv_timeout(Duration::from_secs(INTERVALO_SUPERVISAO_SECS)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if handle.morreu() {
                    log_arquivo(
                        "ERRO",
                        "Thread do agente morreu sem shutdown solicitado. Encerrando com \
                         código 1 para o SCM ressuscitar o processo.",
                    );

                    // Stopped ANTES do exit: se o processo sumir primeiro, o SCM
                    // registra terminação inesperada em vez de parada reportada.
                    //
                    // Isto só recupera porque o failureflag está TRUE. Sem ele, o
                    // SCM só dispara recuperação quando o processo morre SEM
                    // reportar SERVICE_STOPPED — e aqui estamos reportando.
                    let _ = status_handle.set_service_status(ServiceStatus {
                        service_type: ServiceType::OWN_PROCESS,
                        current_state: ServiceState::Stopped,
                        controls_accepted: ServiceControlAccept::empty(),
                        exit_code: ServiceExitCode::Win32(1),
                        checkpoint: 0,
                        wait_hint: Duration::default(),
                        process_id: None,
                    });

                    std::process::exit(1);
                }
            }
        }
    }

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(WAIT_HINT_STOP_SECS),
        process_id: None,
    })?;

    handle.parar();

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}
