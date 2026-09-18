mod agent;
mod executor;
mod gravador;
mod service;
mod shared;

fn main() {
    // PRIMEIRA instrução, antes de qualquer coisa que possa panicar — inclusive
    // carregar_config(), cuja resolução de ProgramData tem unwrap().
    shared::instalar_panic_hook();

    // Só depois do hook: carregar_config passa por raiz_program_data, que tem
    // unwrap() no caminho de fallback.
    let config = shared::carregar_config();

    shared::log_arquivo(
        "INFO",
        &format!(
            "Report Studio Agent (serviço) v{} iniciando.",
            env!("CARGO_PKG_VERSION")
        ),
    );
    shared::log_arquivo("INFO", &config.resumo_limites());
    shared::log_arquivo("INFO", &config.resumo_integracoes());

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("install")   => service::instalar(),
        Some("uninstall") => service::desinstalar(),
        Some("start")     => service::iniciar(),
        Some("stop")      => service::parar(),
        Some("restart")   => service::reiniciar(),
        Some("status")    => service::mostrar_status(),
        _ => {
            if let Err(e) = service::run() {
                eprintln!("Erro ao iniciar serviço: {e}");
                std::process::exit(1);
            }
        }
    }
}
