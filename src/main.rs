mod agent;
mod executor;
mod gravador;
mod gui;
mod shared;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // PRIMEIRA instrução, antes de qualquer coisa que possa panicar — inclusive
    // carregar_config(), cuja resolução de ProgramData tem unwrap().
    shared::instalar_panic_hook();

    // Só depois do hook: carregar_config passa por raiz_program_data, que tem
    // unwrap() no caminho de fallback.
    let config = shared::carregar_config();

    shared::log_arquivo(
        "INFO",
        &format!(
            "Report Studio Agent (GUI) v{} iniciando.",
            env!("CARGO_PKG_VERSION")
        ),
    );
    shared::log_arquivo("INFO", &config.resumo_limites());
    shared::log_arquivo("INFO", &config.resumo_integracoes());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 620.0])
            .with_min_inner_size([440.0, 400.0])
            .with_title("Report Studio Agent"),
        ..Default::default()
    };

    eframe::run_native(
        "Report Studio Agent",
        options,
        Box::new(|cc| Ok(Box::new(gui::AgentApp::new(cc)))),
    )
}
