use eframe::egui;

use crate::agent;
use crate::shared::{carregar_config, salvar_config, AgentConfig, SharedState, Status, SERVICE_NAME};

#[derive(Clone, PartialEq)]
enum StatusServico {
    NaoInstalado,
    Parado,
    Rodando,
    Iniciando,
    Parando,
    Erro(String),
}

impl StatusServico {
    fn label(&self) -> &str {
        match self {
            Self::NaoInstalado => "Não instalado",
            Self::Parado       => "Parado",
            Self::Rodando      => "Rodando",
            Self::Iniciando    => "Iniciando...",
            Self::Parando      => "Parando...",
            Self::Erro(_)      => "Erro",
        }
    }

    fn cor(&self) -> egui::Color32 {
        match self {
            Self::NaoInstalado => egui::Color32::from_rgb(100, 100, 100),
            Self::Parado       => egui::Color32::from_rgb(250, 200, 50),
            Self::Rodando      => egui::Color32::from_rgb(52, 211, 153),
            Self::Iniciando    => egui::Color32::from_rgb(96, 165, 250),
            Self::Parando      => egui::Color32::from_rgb(250, 200, 50),
            Self::Erro(_)      => egui::Color32::from_rgb(248, 113, 113),
        }
    }

    fn rodando(&self) -> bool {
        matches!(self, Self::Rodando | Self::Iniciando)
    }
}

fn consultar_status_servico() -> StatusServico {
    #[cfg(windows)]
    {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let manager = match ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ) {
            Ok(m) => m,
            Err(_) => return StatusServico::NaoInstalado,
        };

        let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(s) => s,
            Err(_) => return StatusServico::NaoInstalado,
        };

        match service.query_status() {
            Ok(status) => {
                use windows_service::service::ServiceState;
                match status.current_state {
                    ServiceState::Stopped      => StatusServico::Parado,
                    ServiceState::Running      => StatusServico::Rodando,
                    ServiceState::StartPending => StatusServico::Iniciando,
                    ServiceState::StopPending  => StatusServico::Parando,
                    _ => StatusServico::Erro("Estado desconhecido".into()),
                }
            }
            Err(e) => StatusServico::Erro(format!("{e}")),
        }
    }

    #[cfg(not(windows))]
    {
        StatusServico::Erro("Disponível apenas no Windows".into())
    }
}

fn executar_servico_elevado(comando: &str) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("agente-service.exe")));

        let exe = match exe {
            Some(p) if p.exists() => p,
            _ => return false,
        };

        let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let args_w: Vec<u16> = std::ffi::OsStr::new(comando)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let runas: Vec<u16> = std::ffi::OsStr::new("runas")
            .encode_wide()
            .chain(Some(0))
            .collect();

        unsafe {
            let result = windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                runas.as_ptr(),
                exe_w.as_ptr(),
                args_w.as_ptr(),
                std::ptr::null(),
                0,
            );
            result as isize > 32
        }
    }

    #[cfg(not(windows))]
    {
        let _ = comando;
        false
    }
}

fn service_exe_existe() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("agente-service.exe")))
        .map(|p| p.exists())
        .unwrap_or(false)
}

pub struct AgentApp {
    url:              String,
    empresa_id:       String,
    token:            String,
    state:            std::sync::Arc<std::sync::Mutex<SharedState>>,
    handle:           Option<agent::AgentHandle>,
    config_aberta:    bool,
    servico_aberto:   bool,
    status_servico:   StatusServico,
    ultima_consulta:  std::time::Instant,
    operacao_msg:     Option<(String, std::time::Instant)>,
    // Valores CRUS do agente.toml, retidos para que um ajuste feito à mão
    // sobreviva a um salvamento pela GUI. Ver AgentConfig::max_consultas.
    max_consultas:    Option<usize>,
    max_exportacoes:  Option<usize>,
}

impl AgentApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        let state = SharedState::novo();
        let config = carregar_config();
        let tem_config = config.valida();
        let status_servico = consultar_status_servico();

        let servico_ativo = status_servico.rodando();

        let mut app = Self {
            max_consultas: config.max_consultas,
            max_exportacoes: config.max_exportacoes,
            url: config.url,
            empresa_id: config.empresa_id,
            token: config.token,
            state,
            handle: None,
            config_aberta: !tem_config,
            servico_aberto: false,
            status_servico,
            ultima_consulta: std::time::Instant::now(),
            operacao_msg: None,
        };

        if tem_config && !servico_ativo {
            app.conectar();
        }

        if servico_ativo {
            let mut s = app.state.lock().unwrap();
            s.log("Serviço Windows ativo — conexão delegada ao serviço.");
            s.status = Status::Conectado;
        }

        app
    }

    fn conectar(&mut self) {
        if self.status_servico.rodando() {
            let mut s = self.state.lock().unwrap();
            s.log("Serviço ativo — GUI não conecta para evitar conflito.");
            return;
        }

        if let Some(h) = self.handle.take() {
            h.parar();
        }

        let config = AgentConfig {
            url: self.url.clone(),
            empresa_id: self.empresa_id.clone(),
            token: self.token.clone(),
            max_consultas: self.max_consultas,
            max_exportacoes: self.max_exportacoes,
        };

        salvar_config(&config);

        let url = config.url.clone();
        let token_completo = config.token_completo();

        {
            let mut s = self.state.lock().unwrap();
            s.status = Status::Conectando;
            s.log("Iniciando conexão...");
        }

        self.handle = Some(agent::AgentHandle::iniciar(
            url,
            token_completo,
            config.limites(),
            self.state.clone(),
        ));
    }

    fn desconectar(&mut self) {
        if let Some(h) = self.handle.take() {
            h.parar();
        }
        let mut s = self.state.lock().unwrap();
        s.status = Status::Desconectado;
        s.log("Desconectado manualmente.");
    }

    fn executar_operacao_servico(&mut self, comando: &str, label: &str) {
        let config = AgentConfig {
            url: self.url.clone(),
            empresa_id: self.empresa_id.clone(),
            token: self.token.clone(),
            max_consultas: self.max_consultas,
            max_exportacoes: self.max_exportacoes,
        };
        salvar_config(&config);

        if executar_servico_elevado(comando) {
            self.operacao_msg = Some((format!("{label}..."), std::time::Instant::now()));

            if comando == "start" || comando == "install" || comando == "restart" {
                if let Some(h) = self.handle.take() {
                    h.parar();
                }
                let mut s = self.state.lock().unwrap();
                s.log(&format!("{label} serviço — GUI desconectada para evitar conflito."));
                s.status = Status::Desconectado;
            }
        } else {
            self.operacao_msg = Some((
                format!("Falha ao {}", label.to_lowercase()),
                std::time::Instant::now(),
            ));
        }
    }

    fn atualizar_status_servico(&mut self) {
        if self.ultima_consulta.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }

        let anterior = self.status_servico.clone();
        self.status_servico = consultar_status_servico();
        self.ultima_consulta = std::time::Instant::now();

        if !anterior.rodando() && self.status_servico.rodando() {
            if let Some(h) = self.handle.take() {
                h.parar();
            }
            let mut s = self.state.lock().unwrap();
            s.log("Serviço iniciou — conexão da GUI encerrada.");
            s.status = Status::Conectado;
        }

        if anterior.rodando() && !self.status_servico.rodando() {
            let mut s = self.state.lock().unwrap();
            s.log("Serviço parou.");
            s.status = Status::Desconectado;
            drop(s);

            let config = AgentConfig {
                url: self.url.clone(),
                empresa_id: self.empresa_id.clone(),
                token: self.token.clone(),
                max_consultas: self.max_consultas,
                max_exportacoes: self.max_exportacoes,
            };

            if config.valida() {
                self.conectar();
            }
        }
    }
}

impl eframe::App for AgentApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        self.atualizar_status_servico();

        if let Some((_, t)) = &self.operacao_msg {
            if t.elapsed() > std::time::Duration::from_secs(4) {
                self.operacao_msg = None;
            }
        }

        let (status, log_count) = {
            let s = self.state.lock().unwrap();
            (s.status.clone(), s.logs.len())
        };

        let servico_ativo = self.status_servico.rodando();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.style_mut().visuals = egui::Visuals::dark();
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);

            ui.horizontal(|ui| {
                ui.heading(
                    egui::RichText::new("Report Studio Agent")
                        .strong()
                        .size(18.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Vem do Cargo.toml para não divergir do que é reportado no
                    // Autenticar e no log de subida.
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .weak()
                            .size(11.0),
                    );
                });
            });

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if servico_ativo {
                    let cor = egui::Color32::from_rgb(52, 211, 153);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 5.0, cor);
                    ui.label(egui::RichText::new("Conectado via Serviço").color(cor).strong());
                } else {
                    let (cor, texto) = match &status {
                        Status::Desconectado => {
                            (egui::Color32::from_rgb(100, 100, 100), "Desconectado")
                        }
                        Status::Conectando => {
                            (egui::Color32::from_rgb(250, 200, 50), "Conectando...")
                        }
                        Status::Conectado => {
                            (egui::Color32::from_rgb(52, 211, 153), "Conectado")
                        }
                        Status::Erro(_) => {
                            (egui::Color32::from_rgb(248, 113, 113), "Erro")
                        }
                    };

                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 5.0, cor);
                    ui.label(egui::RichText::new(texto).color(cor).strong());

                    if let Status::Erro(msg) = &status {
                        ui.label(egui::RichText::new(format!("— {msg}")).weak().size(11.0));
                    }
                }
            });

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);

            // ── Configuração ───────────────────────────────────────────────

            let config_label = if self.config_aberta {
                "▼ Configuração"
            } else {
                "▶ Configuração"
            };

            if ui
                .button(egui::RichText::new(config_label).size(13.0))
                .clicked()
            {
                self.config_aberta = !self.config_aberta;
            }

            if self.config_aberta {
                ui.add_space(4.0);

                egui::Frame::group(ui.style())
                    .inner_margin(12.0)
                    .rounding(8.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("URL do Servidor").weak().size(11.0));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.url)
                                .hint_text("wss://reports.orizoncloud.com.br/ws/agente")
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );

                        ui.add_space(4.0);

                        ui.label(egui::RichText::new("ID da Empresa").weak().size(11.0));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.empresa_id)
                                .hint_text("00000000-0000-0000-0000-000000000001")
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );

                        ui.add_space(4.0);

                        ui.label(egui::RichText::new("Token do Agente").weak().size(11.0));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.token)
                                .hint_text("gdt_xxxxxxxx...")
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace)
                                .password(true),
                        );

                        ui.add_space(8.0);

                        if servico_ativo {
                            ui.colored_label(
                                egui::Color32::from_rgb(250, 200, 50),
                                "Serviço ativo — salvar aplica a nova config reiniciando o serviço.",
                            );

                            ui.add_space(4.0);

                            let btn = egui::Button::new(
                                egui::RichText::new("Salvar e reiniciar serviço")
                                    .color(egui::Color32::WHITE)
                                    .size(12.0),
                            )
                            .fill(egui::Color32::from_rgb(37, 99, 235));

                            if ui.add(btn).clicked() {
                                let config = AgentConfig {
                                    url: self.url.clone(),
                                    empresa_id: self.empresa_id.clone(),
                                    token: self.token.clone(),
                                    max_consultas: self.max_consultas,
                                    max_exportacoes: self.max_exportacoes,
                                };
                                salvar_config(&config);

                                if executar_servico_elevado("restart") {
                                    self.operacao_msg = Some((
                                        "Reiniciando serviço...".into(),
                                        std::time::Instant::now(),
                                    ));
                                    self.state
                                        .lock()
                                        .unwrap()
                                        .log("Configuração salva. Reiniciando serviço para aplicar...");
                                } else {
                                    self.operacao_msg = Some((
                                        "Falha ao reiniciar serviço".into(),
                                        std::time::Instant::now(),
                                    ));
                                    self.state
                                        .lock()
                                        .unwrap()
                                        .log("Config salva, mas falha ao reiniciar o serviço (UAC negado?).");
                                }
                            }
                        } else {
                            ui.horizontal(|ui| {
                                let pode_conectar =
                                    !self.url.is_empty() && !self.token.is_empty();
                                let conectado =
                                    matches!(status, Status::Conectado | Status::Conectando);

                                if conectado {
                                    if ui
                                        .button(
                                            egui::RichText::new("⏹ Desconectar")
                                                .color(egui::Color32::from_rgb(248, 113, 113)),
                                        )
                                        .clicked()
                                    {
                                        self.desconectar();
                                    }
                                } else {
                                    let btn = egui::Button::new(
                                        egui::RichText::new("▶ Salvar e Conectar")
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(egui::Color32::from_rgb(37, 99, 235));

                                    if ui.add_enabled(pode_conectar, btn).clicked() {
                                        self.conectar();
                                    }
                                }

                                if conectado {
                                    if ui.button("↻ Reconectar").clicked() {
                                        self.conectar();
                                    }
                                }
                            });
                        }
                    });
            }

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);

            // ── Serviço Windows ────────────────────────────────────────────

            let servico_label = if self.servico_aberto {
                "▼ Serviço Windows"
            } else {
                "▶ Serviço Windows"
            };

            if ui
                .button(egui::RichText::new(servico_label).size(13.0))
                .clicked()
            {
                self.servico_aberto = !self.servico_aberto;
                if self.servico_aberto {
                    self.status_servico = consultar_status_servico();
                }
            }

            if self.servico_aberto {
                ui.add_space(4.0);

                egui::Frame::group(ui.style())
                    .inner_margin(12.0)
                    .rounding(8.0)
                    .show(ui, |ui| {
                        if !service_exe_existe() {
                            ui.colored_label(
                                egui::Color32::from_rgb(248, 113, 113),
                                "agente-service.exe não encontrado na mesma pasta.",
                            );
                            return;
                        }

                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Status:").weak().size(12.0));

                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(8.0, 8.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(
                                rect.center(),
                                4.0,
                                self.status_servico.cor(),
                            );

                            ui.label(
                                egui::RichText::new(self.status_servico.label())
                                    .color(self.status_servico.cor())
                                    .strong()
                                    .size(12.0),
                            );
                        });

                        if let Some((msg, _)) = &self.operacao_msg {
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(msg)
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(96, 165, 250)),
                            );
                        }

                        ui.add_space(8.0);

                        let instalado = !matches!(
                            self.status_servico,
                            StatusServico::NaoInstalado
                        );
                        let rodando = matches!(
                            self.status_servico,
                            StatusServico::Rodando
                        );
                        let parado = matches!(
                            self.status_servico,
                            StatusServico::Parado
                        );

                        ui.horizontal(|ui| {
                            if !instalado {
                                let config_valida = !self.url.is_empty() && !self.token.is_empty();

                                let btn = egui::Button::new(
                                    egui::RichText::new("Instalar serviço")
                                        .color(egui::Color32::WHITE)
                                        .size(12.0),
                                )
                                .fill(egui::Color32::from_rgb(37, 99, 235));

                                if ui.add_enabled(config_valida, btn).clicked() {
                                    self.executar_operacao_servico("install", "Instalando");
                                }

                                if !config_valida {
                                    ui.label(
                                        egui::RichText::new("Preencha URL e Token primeiro")
                                            .weak()
                                            .size(10.0),
                                    );
                                }
                            }

                            if instalado && !rodando {
                                let btn = egui::Button::new(
                                    egui::RichText::new("▶ Iniciar")
                                        .color(egui::Color32::WHITE)
                                        .size(12.0),
                                )
                                .fill(egui::Color32::from_rgb(22, 163, 74));

                                if ui.add(btn).clicked() {
                                    self.executar_operacao_servico("start", "Iniciando");
                                }
                            }

                            if rodando {
                                let btn = egui::Button::new(
                                    egui::RichText::new("⏹ Parar")
                                        .color(egui::Color32::WHITE)
                                        .size(12.0),
                                )
                                .fill(egui::Color32::from_rgb(220, 38, 38));

                                if ui.add(btn).clicked() {
                                    self.executar_operacao_servico("stop", "Parando");
                                }
                            }

                            if instalado && parado {
                                if ui
                                    .button(
                                        egui::RichText::new("Desinstalar")
                                            .color(egui::Color32::from_rgb(248, 113, 113))
                                            .size(12.0),
                                    )
                                    .clicked()
                                {
                                    self.executar_operacao_servico("uninstall", "Desinstalando");
                                }
                            }
                        });

                        ui.add_space(6.0);

                        ui.label(
                            egui::RichText::new(
                                "O serviço mantém o agente rodando em segundo plano, \
                                 mesmo sem a janela aberta. Inicia automaticamente com o Windows.",
                            )
                            .weak()
                            .size(10.0),
                        );

                        if instalado && rodando {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(
                                    "Dica: com o serviço ativo, você pode fechar esta janela.",
                                )
                                .size(10.0)
                                .color(egui::Color32::from_rgb(52, 211, 153)),
                            );
                        }
                    });
            }

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);

            // ── Logs ───────────────────────────────────────────────────────

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Logs").strong().size(13.0));
                ui.label(
                    egui::RichText::new(format!("({log_count})"))
                        .weak()
                        .size(11.0),
                );

                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.small_button("Limpar").clicked() {
                            self.state.lock().unwrap().logs.clear();
                        }
                    },
                );
            });

            ui.add_space(2.0);

            let logs = {
                let s = self.state.lock().unwrap();
                s.logs.clone()
            };

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in &logs {
                        let color = if line.contains("Erro") || line.contains("Falha") {
                            egui::Color32::from_rgb(248, 113, 113)
                        } else if line.contains("Conectado")
                            || line.contains("Autenticado")
                            || line.contains("Serviço")
                        {
                            egui::Color32::from_rgb(52, 211, 153)
                        } else if line.contains("Executando")
                            || line.contains("Exportando")
                        {
                            egui::Color32::from_rgb(96, 165, 250)
                        } else {
                            egui::Color32::from_rgb(148, 163, 184)
                        };

                        ui.label(
                            egui::RichText::new(line)
                                .font(egui::FontId::monospace(11.0))
                                .color(color),
                        );
                    }
                });
        });
    }
}
