use odbc_api::{buffers::TextRowSet, ConnectionOptions, Cursor, Environment, ResultSetMetadata};
use serde_json::Value as JsonValue;

use crate::shared::mascarar;

const BUFFER_POR_COLUNA: usize = 16384;
const BATCH_SIZE: usize = 1000;

/// Segundos de espera pelo login antes de desistir. Sem isto o default é do
/// driver — no DB2 CLI, tipicamente infinito. Confirmado no fonte do odbc-api
/// 8.1.4 (connection.rs:696-700) e aplicado antes do SQLDriverConnect
/// (environment.rs:275-281).
const LOGIN_TIMEOUT_SECS: u32 = 15;

/// Fronteira de erro do executor. TODO texto de erro que sai daqui passa por
/// aqui, e por isso é mascarado num lugar só.
///
/// Isto não é sobre o arquivo de log — é o caminho mais grave dos dois. Este
/// texto vira `MensagemAgente::Erro { mensagem }`, atravessa a rede, é
/// persistido na coluna `erro_msg` de `jobs_execucao` num Postgres multi-tenant
/// e aparece no navegador do usuário. O diagnóstico do driver DB2 pode ecoar a
/// connection string inteira, com UID e PWD.
///
/// Invariante greppável: não deve existir `format!` produzindo mensagem de erro
/// neste arquivo sem passar por esta função.
fn erro(contexto: &str, detalhe: impl std::fmt::Display) -> String {
    mascarar(&format!("{contexto}: {detalhe}"))
}

fn opcoes_conexao() -> ConnectionOptions {
    ConnectionOptions {
        login_timeout_sec: Some(LOGIN_TIMEOUT_SECS),
        ..Default::default()
    }
}

pub struct ResultadoConsulta {
    pub colunas:           Vec<String>,
    pub linhas:            Vec<Vec<JsonValue>>,
    pub total_linhas:      usize,
    pub tempo_execucao_ms: u64,
}

pub struct ExecutorConsulta {
    environment: Environment,
}

impl ExecutorConsulta {
    pub fn novo() -> Result<Self, String> {
        let env = Environment::new().map_err(|e| erro("Erro ODBC", e))?;
        Ok(Self { environment: env })
    }

    pub fn executar(
        &self,
        connection_string: &str,
        sql: &str,
        limite: usize,
    ) -> Result<ResultadoConsulta, String> {
        let inicio = std::time::Instant::now();

        let conn = self.environment
            .connect_with_connection_string(connection_string, opcoes_conexao())
            .map_err(|e| erro("Falha na conexão", e))?;

        let sql_final = Self::preparar_sql(sql, limite);

        let cursor = conn
            .execute(&sql_final, ())
            .map_err(|e| erro("Erro ao executar", e))?
            .ok_or_else(|| "Query não retornou resultados".to_string())?;

        let capacidade = if limite > 0 { limite } else { 50_000 };
        let (colunas, linhas) = Self::ler_cursor(cursor, capacidade)?;
        let total = linhas.len();

        Ok(ResultadoConsulta {
            colunas,
            linhas,
            total_linhas: total,
            tempo_execucao_ms: inicio.elapsed().as_millis() as u64,
        })
    }

    pub fn descrever(
        &self,
        connection_string: &str,
        sql: &str,
    ) -> Result<Vec<String>, String> {
        let conn = self.environment
            .connect_with_connection_string(connection_string, opcoes_conexao())
            .map_err(|e| erro("Falha na conexão", e))?;

        let mut prepared = conn
            .prepare(sql)
            .map_err(|e| erro("Erro ao preparar SQL", e))?;

        let num_cols = prepared
            .num_result_cols()
            .map_err(|e| erro("Erro ao obter colunas", e))? as u16;

        let mut colunas = Vec::with_capacity(num_cols as usize);
        for i in 1..=num_cols {
            let mut desc = odbc_api::ColumnDescription::default();
            prepared
                .describe_col(i, &mut desc)
                .map_err(|e| erro(&format!("Erro ao descrever coluna {i}"), e))?;
            colunas.push(String::from_utf16_lossy(&desc.name));
        }

        Ok(colunas)
    }

    pub fn exportar_streaming<F>(
        &self,
        connection_string: &str,
        sql: &str,
        chunk_size: usize,
        mut on_chunk: F,
    ) -> Result<(), String>
    where
        F: FnMut(Vec<String>, Vec<Vec<JsonValue>>, usize, bool) -> bool,
    {
        let conn = self.environment
            .connect_with_connection_string(connection_string, opcoes_conexao())
            .map_err(|e| erro("Falha na conexão", e))?;

        let sql_limpo = sql.trim().trim_end_matches(';');
        let mut cursor = conn
            .execute(sql_limpo, ())
            .map_err(|e| erro("Erro ao executar", e))?
            .ok_or_else(|| "Query não retornou resultados".to_string())?;

        let num_cols = cursor
            .num_result_cols()
            .map_err(|e| erro("Erro ao obter colunas", e))? as usize;

        let mut colunas = Vec::with_capacity(num_cols);
        for i in 1..=(num_cols as u16) {
            let mut desc = odbc_api::ColumnDescription::default();
            cursor
                .describe_col(i, &mut desc)
                .map_err(|e| erro(&format!("Erro ao descrever coluna {i}"), e))?;
            colunas.push(String::from_utf16_lossy(&desc.name));
        }

        let mut buffers = TextRowSet::for_cursor(BATCH_SIZE, &mut cursor, Some(BUFFER_POR_COLUNA))
            .map_err(|e| erro("Erro ao criar buffers", e))?;

        let mut row_set = cursor
            .bind_buffer(&mut buffers)
            .map_err(|e| erro("Erro ao vincular buffers", e))?;

        let mut chunk_atual: Vec<Vec<JsonValue>> = Vec::with_capacity(chunk_size);
        let mut chunk_idx = 0_usize;
        let mut primeiro = true;

        while let Some(batch) = row_set
            .fetch()
            .map_err(|e| erro("Erro ao buscar dados", e))?
        {
            for row_idx in 0..batch.num_rows() {
                let linha = Self::ler_linha_batch(&batch, num_cols, row_idx);
                chunk_atual.push(linha);

                if chunk_atual.len() >= chunk_size {
                    let cols = if primeiro { colunas.clone() } else { vec![] };
                    primeiro = false;
                    let continuar = on_chunk(cols, std::mem::take(&mut chunk_atual), chunk_idx, false);
                    chunk_idx += 1;
                    if !continuar {
                        return Ok(());
                    }
                }
            }
        }

        let cols = if primeiro { colunas.clone() } else { vec![] };
        on_chunk(cols, chunk_atual, chunk_idx, true);

        Ok(())
    }

    fn preparar_sql(sql: &str, limite: usize) -> String {
        let sql_limpo = sql.trim().trim_end_matches(';');
        if limite == 0 || sql_limpo.to_uppercase().contains("FETCH FIRST") {
            sql_limpo.to_string()
        } else {
            format!("{sql_limpo} FETCH FIRST {limite} ROWS ONLY")
        }
    }

    fn ler_linha_batch(batch: &TextRowSet, num_cols: usize, row_idx: usize) -> Vec<JsonValue> {
        let mut linha = Vec::with_capacity(num_cols);
        for col_idx in 0..num_cols {
            let valor = match batch.at(col_idx, row_idx) {
                Some(bytes) => Self::converter_valor(bytes),
                None => JsonValue::Null,
            };
            linha.push(valor);
        }
        linha
    }

    fn converter_valor(bytes: &[u8]) -> JsonValue {
        let texto = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        };

        if let Ok(n) = texto.parse::<i64>() {
            JsonValue::from(n)
        } else if let Ok(n) = texto.parse::<f64>() {
            JsonValue::from(n)
        } else {
            JsonValue::String(texto)
        }
    }

    fn ler_cursor(
        mut cursor: impl Cursor,
        capacidade: usize,
    ) -> Result<(Vec<String>, Vec<Vec<JsonValue>>), String> {
        let num_cols = cursor
            .num_result_cols()
            .map_err(|e| erro("Erro ao obter colunas", e))? as usize;

        let mut colunas = Vec::with_capacity(num_cols);
        for i in 1..=(num_cols as u16) {
            let mut desc = odbc_api::ColumnDescription::default();
            cursor
                .describe_col(i, &mut desc)
                .map_err(|e| erro(&format!("Erro ao descrever coluna {i}"), e))?;
            colunas.push(String::from_utf16_lossy(&desc.name));
        }

        let buffers = TextRowSet::for_cursor(BATCH_SIZE, &mut cursor, Some(BUFFER_POR_COLUNA))
            .map_err(|e| erro("Erro ao criar buffers", e))?;

        let mut row_set = cursor
            .bind_buffer(buffers)
            .map_err(|e| erro("Erro ao vincular buffers", e))?;

        let mut linhas: Vec<Vec<JsonValue>> = Vec::with_capacity(capacidade.min(50_000));

        while let Some(batch) = row_set
            .fetch()
            .map_err(|e| erro("Erro ao buscar dados", e))?
        {
            for row_idx in 0..batch.num_rows() {
                let linha = Self::ler_linha_batch(&batch, num_cols, row_idx);
                linhas.push(linha);
            }
        }

        Ok((colunas, linhas))
    }
}