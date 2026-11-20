//! Agente na nuvem: o mesmo agente do Windows, sem janela, rodando no servidor do Report.
//!
//! Para clientes com banco na nuvem (DataCISS): eles não têm máquina na rede do banco onde instalar o
//! agente, mas o banco aceita conexão do servidor. Este processo abre uma conexão com o Report por
//! empresa (cada uma com o seu token) e executa as consultas pelo driver ODBC/CLI da IBM no Linux.
//! Para o Report é um agente como outro qualquer: nada muda nele.
//!
//! Configuração só por ambiente (container):
//! - `AGENTE_TOKENS`: um `empresa_id:token` por empresa, separados por vírgula, espaço ou linha.
//! - `AGENTE_URL`: endereço do WebSocket do Report (padrão: a API na rede interna do Docker).
//! - `AGENTE_MAX_CONSULTAS` / `AGENTE_MAX_EXPORTACOES`: limites por empresa (opcional).

// Os módulos são os mesmos dos binários do Windows; o que só o serviço e a janela usam fica sem uso aqui.
#![allow(dead_code)]

mod agent;
mod executor;
mod gravador;
mod shared;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::AgentHandle;
use shared::{AgentConfig, SharedState};

const URL_PADRAO: &str = "ws://analytics-api:3000/ws/agente";
const INTERVALO_SUPERVISAO: Duration = Duration::from_secs(2);

struct Instancia {
    config: AgentConfig,
    estado: Arc<Mutex<SharedState>>,
    handle: Option<AgentHandle>,
    linhas_mostradas: usize,
}

impl Instancia {
    fn subir(&mut self) {
        self.handle = Some(AgentHandle::iniciar(
            self.config.url.clone(),
            self.config.token_completo(),
            self.config.limites(),
            self.config.pastas(),
            Arc::clone(&self.estado),
        ));
    }

    /// Repassa para a saída padrão (log do container) o que o agente registrou desde a última volta,
    /// com a empresa na frente: várias empresas escrevem no mesmo log.
    fn despejar_logs(&mut self) {
        let Ok(mut estado) = self.estado.lock() else { return };
        // O SharedState descarta as 100 linhas mais antigas ao passar de 500; recomeça a contagem.
        if self.linhas_mostradas > estado.logs.len() {
            self.linhas_mostradas = 0;
        }
        for linha in &estado.logs[self.linhas_mostradas..] {
            println!("[{}] {linha}", self.config.empresa_id);
        }
        self.linhas_mostradas = estado.logs.len();
        // Sem janela, ninguém lê o histórico em memória: mantê-lo curto evita o descarte acima.
        if estado.logs.len() > 200 {
            estado.logs.clear();
            self.linhas_mostradas = 0;
        }
    }
}

fn limite(nome: &str) -> Option<usize> {
    std::env::var(nome).ok().and_then(|v| v.trim().parse().ok())
}

/// `empresa_id:token` → config. O id vem antes dos dois-pontos; o token é o resto.
fn config_do_token(bruto: &str, url: &str) -> Option<AgentConfig> {
    let (empresa_id, token) = bruto.split_once(':')?;
    let (empresa_id, token) = (empresa_id.trim(), token.trim());
    if empresa_id.is_empty() || token.is_empty() {
        return None;
    }
    Some(AgentConfig {
        url: url.to_string(),
        empresa_id: empresa_id.to_string(),
        token: token.to_string(),
        max_consultas: limite("AGENTE_MAX_CONSULTAS"),
        max_exportacoes: limite("AGENTE_MAX_EXPORTACOES"),
        // Entrega de arquivo em pasta local (eSales/NeoGrid) não existe no servidor.
        pasta_integracoes: None,
        pasta_neogrid: None,
    })
}

fn main() {
    shared::instalar_panic_hook();

    let url = std::env::var("AGENTE_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| URL_PADRAO.to_string());
    let brutos = std::env::var("AGENTE_TOKENS").unwrap_or_default();
    let mut instancias: Vec<Instancia> = Vec::new();
    for bruto in brutos.split([',', ';', ' ', '\n', '\t']).filter(|t| !t.trim().is_empty()) {
        match config_do_token(bruto, &url) {
            Some(config) => instancias.push(Instancia { config, estado: SharedState::novo(), handle: None, linhas_mostradas: 0 }),
            None => eprintln!("Token ignorado: formato esperado empresa_id:token"),
        }
    }
    if instancias.is_empty() {
        eprintln!("Nenhum token em AGENTE_TOKENS (empresa_id:token, separados por vírgula). Nada a fazer.");
        std::process::exit(1);
    }

    println!(
        "Report Studio Agent na nuvem v{} — {} empresa(s), servidor {url}",
        env!("CARGO_PKG_VERSION"),
        instancias.len()
    );
    for instancia in &mut instancias {
        println!("[{}] {}", instancia.config.empresa_id, instancia.config.resumo_limites());
        instancia.subir();
    }

    // O laço do agente reconecta sozinho; aqui só se cobre a thread morrer (panic, runtime).
    loop {
        std::thread::sleep(INTERVALO_SUPERVISAO);
        for instancia in &mut instancias {
            instancia.despejar_logs();
            if instancia.handle.as_ref().is_some_and(|h| h.morreu()) {
                println!("[{}] agente parou inesperadamente; subindo de novo", instancia.config.empresa_id);
                instancia.subir();
            }
        }
    }
}

#[cfg(test)]
mod testes {
    use super::config_do_token;

    #[test]
    fn token_vira_config_da_empresa() {
        let c = config_do_token(" 766cfb19-6e7a:abc123 ", "ws://x/ws/agente").unwrap();
        assert_eq!(c.empresa_id, "766cfb19-6e7a");
        assert_eq!(c.token_completo(), "766cfb19-6e7a:abc123");
        assert!(c.pastas().esales.is_none() && c.pastas().neogrid.is_none());
    }

    #[test]
    fn token_sem_empresa_ou_vazio_e_recusado() {
        for ruim in ["semdoispontos", ":abc", "empresa:", ""] {
            assert!(config_do_token(ruim, "ws://x").is_none(), "{ruim}");
        }
    }
}
