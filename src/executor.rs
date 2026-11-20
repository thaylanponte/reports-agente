use std::sync::OnceLock;

use odbc_api::{
    buffers::{Indicator, TextRowSet},
    handles::AsStatementRef,
    BlockCursor, ConnectionOptions, Cursor, CursorImpl, DataType, Environment, ResultSetMetadata, RowSetBuffer,
};
use serde_json::Value as JsonValue;

use crate::shared::mascarar;

const BUFFER_POR_COLUNA: usize = 16384;
const BATCH_SIZE: usize = 1000;

/// Driver DB2 do Linux (agente na nuvem, `AGENTE_DB2_LINUX=1` na imagem). O ODBC/CLI da IBM tem duas
/// diferenças do driver do Windows que o odbc-api não espera:
///
/// 1. Acha que o indicador de tamanho/nulo tem 4 bytes (o unixODBC e o odbc-api usam 8). No lote, grava
///    os indicadores colados de 4 em 4 bytes: o da linha `k` fica na metade de baixo (k par) ou de cima
///    (k ímpar) do indicador de 8 bytes da posição `k / 2`. Lido do jeito normal, a 1ª linha vinha com o
///    texto cheio de \0 e a 2ª toda nula. Ver `indicador_da_linha`.
/// 2. Fecha o cursor sozinho ao entregar a última linha; fechar de novo dá "cursor inválido" e o
///    odbc-api entra em pânico, deixando a consulta sem resposta. Ver `encerrar_leitura`.
///
/// No Windows nada disso acontece e nada muda.
fn db2_linux() -> bool {
    static LIGADO: OnceLock<bool> = OnceLock::new();
    *LIGADO.get_or_init(|| std::env::var("AGENTE_DB2_LINUX").is_ok_and(|v| v.trim() == "1"))
}

/// Fim da leitura de um cursor. No DB2 do Linux solta os buffers e libera o comando sem o
/// `SQLCloseCursor` (o `SQLFreeHandle` fecha o que tiver sobrado); nos demais, o descarte normal.
fn encerrar_leitura<S: AsStatementRef, B: RowSetBuffer>(leitura: BlockCursor<CursorImpl<S>, B>) {
    if !db2_linux() {
        return;
    }
    if let Ok((cursor, _buffers)) = leitura.unbind() {
        drop(cursor.into_stmt());
    }
}

/// Buffers de texto da leitura. No DB2 do Linux o tamanho sai do tipo do `SQLDescribeCol`: o
/// `SQLColAttribute` que o `for_cursor` usa grava 4 bytes num campo de 8, e tipo negativo (BIGINT = -5)
/// vira 4294967291 e derruba o odbc-api. Mesmo cálculo do `for_cursor` (tamanho UTF-8 com teto).
fn criar_buffers(
    cursor: &mut impl ResultSetMetadata,
    tipos: &[DataType],
) -> Result<TextRowSet, odbc_api::Error> {
    if !db2_linux() {
        return TextRowSet::for_cursor(BATCH_SIZE, cursor, Some(BUFFER_POR_COLUNA));
    }
    TextRowSet::from_max_str_lens(BATCH_SIZE, tipos.iter().map(tamanho_do_buffer))
}

fn tamanho_do_buffer(tipo: &DataType) -> usize {
    tipo.utf8_len().map_or(BUFFER_POR_COLUNA, |n| n.get().min(BUFFER_POR_COLUNA))
}

const SQL_NULL_DATA: i32 = -1;
const SQL_NO_TOTAL: i32 = -4;

/// Indicador de 4 bytes da linha `linha`, tirado do indicador de 8 bytes onde o driver o gravou.
fn indicador_da_linha(par: Indicator, linha: usize) -> i32 {
    let bruto: i64 = match par {
        Indicator::Null => SQL_NULL_DATA as i64,
        Indicator::NoTotal => SQL_NO_TOTAL as i64,
        Indicator::Length(n) => n as i64,
    };
    if linha % 2 == 0 {
        bruto as i32
    } else {
        (bruto >> 32) as i32
    }
}

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
        let mut tipos = Vec::with_capacity(num_cols);
        for i in 1..=(num_cols as u16) {
            let mut desc = odbc_api::ColumnDescription::default();
            cursor
                .describe_col(i, &mut desc)
                .map_err(|e| erro(&format!("Erro ao descrever coluna {i}"), e))?;
            colunas.push(String::from_utf16_lossy(&desc.name));
            tipos.push(desc.data_type);
        }

        let mut buffers = criar_buffers(&mut cursor, &tipos)
            .map_err(|e| erro("Erro ao criar buffers", e))?;

        let mut row_set = cursor
            .bind_buffer(&mut buffers)
            .map_err(|e| erro("Erro ao vincular buffers", e))?;

        let mut chunk_atual: Vec<Vec<JsonValue>> = Vec::with_capacity(chunk_size);
        let mut chunk_idx = 0_usize;
        let mut primeiro = true;

        let mut interrompido = false;
        let mut falha = None;
        'leitura: loop {
            let batch = match row_set.fetch() {
                Ok(Some(batch)) => batch,
                Ok(None) => break,
                Err(e) => {
                    falha = Some(erro("Erro ao buscar dados", e));
                    break;
                }
            };
            for row_idx in 0..batch.num_rows() {
                let linha = Self::ler_linha_batch(&batch, num_cols, row_idx);
                chunk_atual.push(linha);

                if chunk_atual.len() >= chunk_size {
                    let cols = if primeiro { colunas.clone() } else { vec![] };
                    primeiro = false;
                    let continuar = on_chunk(cols, std::mem::take(&mut chunk_atual), chunk_idx, false);
                    chunk_idx += 1;
                    if !continuar {
                        interrompido = true;
                        break 'leitura;
                    }
                }
            }
        }
        encerrar_leitura(row_set);
        if let Some(e) = falha {
            return Err(e);
        }
        if interrompido {
            return Ok(());
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
            let valor = if db2_linux() {
                Self::valor_indicador_32(batch, col_idx, row_idx)
            } else {
                match batch.at(col_idx, row_idx) {
                    Some(bytes) => Self::converter_valor(bytes),
                    None => JsonValue::Null,
                }
            };
            linha.push(valor);
        }
        linha
    }

    /// Valor da célula lendo o indicador de 4 bytes (ver `db2_linux`) e o buffer cru da coluna.
    fn valor_indicador_32(batch: &TextRowSet, col_idx: usize, row_idx: usize) -> JsonValue {
        let tamanho = match indicador_da_linha(batch.indicator_at(col_idx, row_idx / 2), row_idx) {
            SQL_NULL_DATA => return JsonValue::Null,
            SQL_NO_TOTAL => usize::MAX,
            n if n < 0 => return JsonValue::Null,
            n => n as usize,
        };
        let coluna = batch.column(col_idx);
        let largura = coluna.max_len() + 1;
        let inicio = row_idx * largura;
        let buffer = coluna.raw_value_buffer();
        let fim = (inicio + tamanho.min(coluna.max_len())).min(buffer.len());
        Self::converter_valor(&buffer[inicio.min(fim)..fim])
    }

    fn converter_valor(bytes: &[u8]) -> JsonValue {
        // O DB2 CLI do Linux (agente na nuvem) devolve o texto terminado em \0 com o tamanho da coluna
        // inteira no indicador. Texto de banco não tem \0 legítimo: corta no primeiro. No Windows nada muda.
        let bytes = bytes.iter().position(|&b| b == 0).map_or(bytes, |fim| &bytes[..fim]);
        let texto = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        };

        JsonValue::String(texto)
    }

    fn ler_cursor<S: AsStatementRef>(
        mut cursor: CursorImpl<S>,
        capacidade: usize,
    ) -> Result<(Vec<String>, Vec<Vec<JsonValue>>), String> {
        let num_cols = cursor
            .num_result_cols()
            .map_err(|e| erro("Erro ao obter colunas", e))? as usize;

        let mut colunas = Vec::with_capacity(num_cols);
        let mut tipos = Vec::with_capacity(num_cols);
        for i in 1..=(num_cols as u16) {
            let mut desc = odbc_api::ColumnDescription::default();
            cursor
                .describe_col(i, &mut desc)
                .map_err(|e| erro(&format!("Erro ao descrever coluna {i}"), e))?;
            colunas.push(String::from_utf16_lossy(&desc.name));
            tipos.push(desc.data_type);
        }

        let buffers = criar_buffers(&mut cursor, &tipos)
            .map_err(|e| erro("Erro ao criar buffers", e))?;

        let mut row_set = cursor
            .bind_buffer(buffers)
            .map_err(|e| erro("Erro ao vincular buffers", e))?;

        let mut linhas: Vec<Vec<JsonValue>> = Vec::with_capacity(capacidade.min(50_000));

        // Erro no meio da leitura (ex.: estouro numérico) também passa por `encerrar_leitura`:
        // o DB2 do Linux já fechou o cursor e o descarte normal entraria em pânico.
        let mut falha = None;
        loop {
            match row_set.fetch() {
                Ok(Some(batch)) => {
                    for row_idx in 0..batch.num_rows() {
                        linhas.push(Self::ler_linha_batch(&batch, num_cols, row_idx));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    falha = Some(erro("Erro ao buscar dados", e));
                    break;
                }
            }
        }
        encerrar_leitura(row_set);

        match falha {
            Some(e) => Err(e),
            None => Ok((colunas, linhas)),
        }
    }
}
#[cfg(test)]
mod testes {
    use super::{indicador_da_linha, tamanho_do_buffer, ExecutorConsulta, BUFFER_POR_COLUNA, SQL_NULL_DATA};
    use odbc_api::DataType;
    use std::num::NonZeroUsize;
    use odbc_api::buffers::Indicator;
    use serde_json::json;

    #[test]
    fn indicadores_de_4_bytes_colados_viram_um_por_linha() {
        // Linhas 0 e 1 com tamanhos 34 e 7: o driver grava [34, 7] colados num indicador de 8 bytes.
        let par = Indicator::Length((7_u64 << 32 | 34) as usize);
        assert_eq!(indicador_da_linha(par, 0), 34);
        assert_eq!(indicador_da_linha(par, 1), 7);
        // Linha 0 nula e linha 1 com 5: [-1, 5].
        let par = Indicator::Length(((5_u64 << 32) | 0xFFFF_FFFF) as usize);
        assert_eq!(indicador_da_linha(par, 0), SQL_NULL_DATA);
        assert_eq!(indicador_da_linha(par, 1), 5);
        // As duas nulas: [-1, -1] é o -1 de 8 bytes, que o odbc-api já entrega como Null.
        assert_eq!(indicador_da_linha(Indicator::Null, 0), SQL_NULL_DATA);
        assert_eq!(indicador_da_linha(Indicator::Null, 1), SQL_NULL_DATA);
    }

    #[test]
    fn texto_do_db2_no_linux_perde_o_preenchimento_nulo() {
        assert_eq!(ExecutorConsulta::converter_valor("Comac Importação\0\0\0".as_bytes()), json!("Comac Importação"));
        assert_eq!(ExecutorConsulta::converter_valor(b"1\0\0\0\0\0\0\0\0\0\0"), json!("1"));
    }

    #[test]
    fn texto_sem_nulo_fica_igual() {
        assert_eq!(ExecutorConsulta::converter_valor(b"12.50"), json!("12.50"));
        assert_eq!(ExecutorConsulta::converter_valor(b""), json!(""));
    }

    #[test]
    fn buffer_pelo_tipo_descrito() {
        assert_eq!(tamanho_do_buffer(&DataType::Varchar { length: NonZeroUsize::new(10) }), 40);
        assert_eq!(tamanho_do_buffer(&DataType::BigInt), 20);
        assert_eq!(tamanho_do_buffer(&DataType::Varchar { length: NonZeroUsize::new(32_000) }), BUFFER_POR_COLUNA);
        assert_eq!(tamanho_do_buffer(&DataType::Unknown), BUFFER_POR_COLUNA);
    }
}
