use std::io::Write;
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use sha2::{Digest, Sha256};

/// Contrato externo, não preferência: o Client da eSales só transmite `.txt`.
/// Recusar o resto impede que um nome vindo do servidor deposite qualquer outra
/// coisa numa pasta que um programa de terceiro varre sozinho.
const EXTENSAO: &str = ".txt";
const NOME_MAX: usize = 255;
const INVALIDOS: &[char] = &['<', '>', '"', '|', '?', '*'];

#[derive(Debug)]
pub struct Gravacao {
    pub caminho: String,
    pub sha256: String,
    pub bytes: u64,
}

pub fn validar_nome(nome: &str) -> Result<(), String> {
    if nome.is_empty() {
        return Err("nome de arquivo vazio".to_string());
    }

    if nome.len() > NOME_MAX {
        return Err(format!("nome de arquivo excede {NOME_MAX} caracteres"));
    }

    if nome.contains('/') || nome.contains('\\') {
        return Err(format!("nome '{nome}' contém separador de caminho"));
    }

    if nome.contains(':') {
        return Err(format!("nome '{nome}' contém ':'"));
    }

    if nome == "." || nome == ".." {
        return Err(format!("nome '{nome}' é referência a diretório"));
    }

    if nome.chars().any(|c| INVALIDOS.contains(&c) || (c as u32) < 0x20) {
        return Err(format!("nome '{nome}' contém caractere inválido"));
    }

    if !nome.to_ascii_lowercase().ends_with(EXTENSAO) {
        return Err(format!("nome '{nome}' não termina em {EXTENSAO}"));
    }

    if nome.len() == EXTENSAO.len() {
        return Err(format!("nome '{nome}' não tem parte antes de {EXTENSAO}"));
    }

    if dispositivo_dos(nome) {
        return Err(format!("nome '{nome}' é dispositivo reservado do Windows"));
    }

    Ok(())
}

fn dispositivo_dos(nome: &str) -> bool {
    let base = nome
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();

    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }

    let b = base.as_bytes();
    b.len() == 4
        && (base.starts_with("COM") || base.starts_with("LPT"))
        && b[3].is_ascii_digit()
        && b[3] != b'0'
}

pub fn gravar(
    pasta: Option<&Path>,
    id: &str,
    nome: &str,
    conteudo_b64: &str,
) -> Result<Gravacao, String> {
    let pasta = pasta.ok_or_else(|| {
        "pasta de integrações não configurada: defina pasta_integracoes no agente.toml".to_string()
    })?;

    validar_nome(nome)?;

    if !pasta.is_dir() {
        return Err(format!(
            "pasta de integrações '{}' não existe ou não é um diretório",
            pasta.display()
        ));
    }

    let bytes = BASE64
        .decode(conteudo_b64.as_bytes())
        .map_err(|e| format!("conteúdo base64 inválido: {e}"))?;

    // Três invariantes desta escrita que nenhum teste aqui consegue quebrar, e
    // que por isso morrem em silêncio se alguém "arrumar" o código:
    //
    // 1. O temporário fica na PASTA DE DESTINO. `rename` só é atômico dentro do
    //    mesmo volume; movê-lo para um spool em %ProgramData% o transformaria em
    //    copiar-e-apagar, e o Client poderia ver o arquivo crescendo.
    // 2. A extensão do temporário NÃO pode ser `.txt`. O Client varre a pasta
    //    por conta própria e recolheria o parcial como se fosse entrega boa.
    // 3. `sync_all` ANTES do `rename` (ver `escrever`). Invertida a ordem, todo
    //    teste continua passando e só uma queda de energia revela o problema —
    //    um arquivo com o nome final e conteúdo truncado, que é exatamente o
    //    caso que a escrita atômica existe para impedir, e o pior de todos,
    //    porque o Client entrega e a Lactalis recebe um arquivo vazio como bom.
    let destino = pasta.join(nome);
    let temporario = pasta.join(format!("{nome}.{id}.parcial"));

    if let Err(e) = escrever(&temporario, &bytes) {
        let _ = std::fs::remove_file(&temporario);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&temporario, &destino) {
        let _ = std::fs::remove_file(&temporario);
        return Err(format!(
            "falha ao renomear para '{}': {e}",
            destino.display()
        ));
    }

    conferir(&destino)
}

fn escrever(temporario: &PathBuf, bytes: &[u8]) -> Result<(), String> {
    let mut arquivo = std::fs::File::create(temporario)
        .map_err(|e| format!("falha ao criar '{}': {e}", temporario.display()))?;

    arquivo
        .write_all(bytes)
        .map_err(|e| format!("falha ao gravar '{}': {e}", temporario.display()))?;

    arquivo
        .sync_all()
        .map_err(|e| format!("falha ao sincronizar '{}': {e}", temporario.display()))
}

/// Relê do DISCO em vez de usar o buffer que já está em memória.
///
/// Os bytes são os mesmos nos dois caminhos, então nenhum teste distingue — mas
/// o significado da confirmação muda. Hash do buffer prova que a rede funcionou;
/// hash do arquivo relido prova que o disco aceitou, que é a única coisa que o
/// servidor pode chamar de entregue ao marcar o arquivo como enviado.
fn conferir(destino: &PathBuf) -> Result<Gravacao, String> {
    let gravado = std::fs::read(destino)
        .map_err(|e| format!("falha ao reler '{}': {e}", destino.display()))?;

    Ok(Gravacao {
        caminho: destino.to_string_lossy().to_string(),
        sha256: data_encoding::HEXLOWER.encode(&Sha256::digest(&gravado)),
        bytes: gravado.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aceita_nome_do_layout_colecta() {
        assert!(validar_nome("ACC_SELLOUT_20260918.txt").is_ok());
        assert!(validar_nome("ACC_CADPROD_20260918.TXT").is_ok());
    }

    #[test]
    fn recusa_separador_de_caminho() {
        assert!(validar_nome("sub/ACC.txt").is_err());
        assert!(validar_nome("sub\\ACC.txt").is_err());
        assert!(validar_nome("\\\\servidor\\share\\ACC.txt").is_err());
    }

    #[test]
    fn recusa_travessia_e_raiz() {
        assert!(validar_nome("..").is_err());
        assert!(validar_nome(".").is_err());
        assert!(validar_nome("../ACC.txt").is_err());
        assert!(validar_nome("..\\..\\Windows\\System32\\ACC.txt").is_err());
    }

    #[test]
    fn recusa_letra_de_unidade_e_ads() {
        assert!(validar_nome("C:ACC.txt").is_err());
        assert!(validar_nome("C:\\ACC.txt").is_err());
        assert!(validar_nome("ACC.txt:oculto").is_err());
    }

    #[test]
    fn recusa_extensao_diferente_de_txt() {
        assert!(validar_nome("agente-service.exe").is_err());
        assert!(validar_nome("ACC_SELLOUT_20260918").is_err());
        assert!(validar_nome("ACC.txt.exe").is_err());
        assert!(validar_nome(".txt").is_err());
    }

    #[test]
    fn recusa_dispositivo_reservado() {
        assert!(validar_nome("NUL.txt").is_err());
        assert!(validar_nome("con.txt").is_err());
        assert!(validar_nome("COM1.txt").is_err());
        assert!(validar_nome("LPT9.txt").is_err());
        assert!(validar_nome("COM0.txt").is_ok());
        assert!(validar_nome("CONTRATO.txt").is_ok());
    }

    #[test]
    fn recusa_caractere_invalido_e_controle() {
        assert!(validar_nome("ACC*.txt").is_err());
        assert!(validar_nome("ACC?.txt").is_err());
        assert!(validar_nome("ACC\n.txt").is_err());
    }

    #[test]
    fn recusa_vazio_e_nome_longo() {
        assert!(validar_nome("").is_err());
        assert!(validar_nome(&format!("{}.txt", "a".repeat(NOME_MAX))).is_err());
    }

    #[test]
    fn pasta_ausente_e_erro_nomeado() {
        let erro = gravar(None, "id", "ACC.txt", "").unwrap_err();
        assert!(erro.contains("pasta_integracoes"));
    }

    #[test]
    fn base64_invalido_e_erro_nomeado() {
        let dir = std::env::temp_dir().join("rs-agente-b64");
        std::fs::create_dir_all(&dir).unwrap();
        let erro = gravar(Some(&dir), "id", "ACC.txt", "!!!nao-e-base64!!!").unwrap_err();
        assert!(erro.contains("base64"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grava_bytes_crus_e_confere_sha() {
        let dir = std::env::temp_dir().join("rs-agente-grava");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let latin1 = vec![b'A', 0xC7, 0xC3, b'O', b'\r', b'\n'];
        let g = gravar(Some(&dir), "id-1", "ACC.txt", &BASE64.encode(&latin1)).unwrap();

        assert_eq!(g.bytes, latin1.len() as u64);
        assert_eq!(std::fs::read(&g.caminho).unwrap(), latin1);
        assert_eq!(
            g.sha256,
            data_encoding::HEXLOWER.encode(&Sha256::digest(&latin1))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sobrescreve_arquivo_existente_e_nao_deixa_parcial() {
        let dir = std::env::temp_dir().join("rs-agente-sobrescreve");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        gravar(Some(&dir), "id-1", "ACC.txt", &BASE64.encode(b"velho")).unwrap();
        let g = gravar(Some(&dir), "id-2", "ACC.txt", &BASE64.encode(b"novo")).unwrap();

        assert_eq!(std::fs::read(&g.caminho).unwrap(), b"novo");

        let restantes: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(restantes, vec!["ACC.txt".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
