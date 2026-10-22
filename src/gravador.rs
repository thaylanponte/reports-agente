use std::io::Write;
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use sha2::{Digest, Sha256};

/// Contrato externo, não preferência: o Client da eSales só transmite `.txt`, e
/// o layout Neogrid DAD também sai em `.txt`. Recusar o resto impede que um
/// nome vindo do servidor deposite qualquer outra coisa numa pasta que um
/// programa de terceiro varre sozinho.
const EXTENSAO: &str = ".txt";
const NOME_MAX: usize = 255;
const INVALIDOS: &[char] = &['<', '>', '"', '|', '?', '*'];
const PASTA_TEMPORARIA: &str = "report-studio-parcial";

/// Programa de terceiro que recolhe o arquivo. Cada um varre a SUA pasta, e a
/// pasta é configurada por destino no agente.toml.
///
/// Os arquivos da NeoGrid saem com o mesmo nome dos da eSales
/// (`ACC_SELLOUT_AAAAMMDD.txt`), então trocar o destino não falharia: gravaria
/// por cima da entrega da Lactalis do mesmo dia. Por isso destino desconhecido
/// é erro, nunca padrão.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destino {
    Esales,
    Neogrid,
}

impl Destino {
    pub fn da_chave(chave: &str) -> Result<Destino, String> {
        match chave {
            "esales" => Ok(Destino::Esales),
            "neogrid" => Ok(Destino::Neogrid),
            outra => Err(format!(
                "destino de entrega '{outra}' desconhecido por este agente — atualize o agente"
            )),
        }
    }

    pub fn chave_config(self) -> &'static str {
        match self {
            Destino::Esales => "pasta_integracoes",
            Destino::Neogrid => "pasta_neogrid",
        }
    }

    /// Onde o temporário é escrito antes do `rename`.
    ///
    /// eSales: na própria pasta, com extensão que não é `.txt` — o Client dela
    /// só recolhe `.txt`, e é assim desde a 0.6.0 em produção.
    ///
    /// NeoGrid: numa pasta IRMÃ da de destino. Não há garantia de que o Client
    /// da NeoGrid filtre por extensão, e se ele recolher tudo o que está na
    /// `out` levaria o parcial. Irmã, e não subpasta, para não depender de ele
    /// não varrer recursivamente. Mesmo pai implica mesmo volume, que é o que
    /// mantém o `rename` atômico.
    fn pasta_temporaria(self, pasta: &Path) -> Result<PathBuf, String> {
        match self {
            Destino::Esales => Ok(pasta.to_path_buf()),
            Destino::Neogrid => {
                let pai = pasta.parent().ok_or_else(|| {
                    format!(
                        "pasta '{}' não tem pasta-mãe para o arquivo temporário",
                        pasta.display()
                    )
                })?;
                let temporaria = pai.join(PASTA_TEMPORARIA);
                std::fs::create_dir_all(&temporaria)
                    .map_err(|e| format!("falha ao criar '{}': {e}", temporaria.display()))?;
                Ok(temporaria)
            }
        }
    }
}

/// Pastas de entrega configuradas, uma por destino.
#[derive(Debug, Clone, Default)]
pub struct Pastas {
    pub esales: Option<PathBuf>,
    pub neogrid: Option<PathBuf>,
}

impl Pastas {
    pub fn de(&self, destino: Destino) -> Option<&Path> {
        match destino {
            Destino::Esales => self.esales.as_deref(),
            Destino::Neogrid => self.neogrid.as_deref(),
        }
    }
}

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
    destino: Destino,
    pasta: Option<&Path>,
    id: &str,
    nome: &str,
    conteudo_b64: &str,
) -> Result<Gravacao, String> {
    let pasta = pasta.ok_or_else(|| {
        format!(
            "pasta de integrações não configurada: defina {} no agente.toml",
            destino.chave_config()
        )
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
    // 1. O temporário fica no MESMO VOLUME do destino (ver `pasta_temporaria`).
    //    `rename` só é atômico dentro do volume; movê-lo para um spool em
    //    %ProgramData% o transformaria em copiar-e-apagar, e o Client poderia
    //    ver o arquivo crescendo.
    // 2. O temporário nunca é algo que o Client recolha: na eSales a extensão
    //    não é `.txt`; na NeoGrid ele nem fica na pasta varrida.
    // 3. `sync_all` ANTES do `rename` (ver `escrever`). Invertida a ordem, todo
    //    teste continua passando e só uma queda de energia revela o problema —
    //    um arquivo com o nome final e conteúdo truncado, que é exatamente o
    //    caso que a escrita atômica existe para impedir, e o pior de todos,
    //    porque o Client entrega e a indústria recebe um arquivo vazio como bom.
    let final_ = pasta.join(nome);
    let temporario = destino
        .pasta_temporaria(pasta)?
        .join(format!("{nome}.{id}.parcial"));

    let conferido = escrever(&temporario, &bytes).and_then(|()| conferir(&temporario));
    let (sha256, tamanho) = match conferido {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&temporario);
            return Err(e);
        }
    };

    if let Err(e) = std::fs::rename(&temporario, &final_) {
        let _ = std::fs::remove_file(&temporario);
        return Err(format!(
            "falha ao renomear para '{}': {e}",
            final_.display()
        ));
    }

    Ok(Gravacao {
        caminho: final_.to_string_lossy().to_string(),
        sha256,
        bytes: tamanho,
    })
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
///
/// Relê o TEMPORÁRIO, antes do `rename`, e não o arquivo final. Depois do
/// `rename` o Client já pode ter recolhido e movido o arquivo; a releitura
/// falharia, o servidor receberia erro de uma entrega que aconteceu, e a nova
/// tentativa mandaria o arquivo em dobro para a indústria.
fn conferir(temporario: &PathBuf) -> Result<(String, u64), String> {
    let gravado = std::fs::read(temporario)
        .map_err(|e| format!("falha ao reler '{}': {e}", temporario.display()))?;

    Ok((
        data_encoding::HEXLOWER.encode(&Sha256::digest(&gravado)),
        gravado.len() as u64,
    ))
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
        let erro = gravar(Destino::Esales, None, "id", "ACC.txt", "").unwrap_err();
        assert!(erro.contains("pasta_integracoes"));

        let erro = gravar(Destino::Neogrid, None, "id", "ACC.txt", "").unwrap_err();
        assert!(erro.contains("pasta_neogrid"));
    }

    #[test]
    fn destino_desconhecido_e_erro_e_nao_cai_na_esales() {
        assert_eq!(Destino::da_chave("esales").unwrap(), Destino::Esales);
        assert_eq!(Destino::da_chave("neogrid").unwrap(), Destino::Neogrid);
        for chave in ["", "ESALES", "Neogrid", "outro"] {
            assert!(Destino::da_chave(chave).is_err(), "{chave}");
        }
    }

    #[test]
    fn neogrid_sem_pasta_nao_usa_a_da_esales() {
        let dir = std::env::temp_dir().join("rs-agente-so-esales");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let pastas = Pastas {
            esales: Some(dir.clone()),
            neogrid: None,
        };
        let erro = gravar(
            Destino::Neogrid,
            pastas.de(Destino::Neogrid),
            "id",
            "ACC.txt",
            &BASE64.encode(b"x"),
        )
        .unwrap_err();

        assert!(erro.contains("pasta_neogrid"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn neogrid_grava_na_out_e_parcial_fica_fora_dela() {
        let raiz = std::env::temp_dir().join("rs-agente-neogrid");
        let _ = std::fs::remove_dir_all(&raiz);
        let out = raiz.join("documents").join("out");
        std::fs::create_dir_all(&out).unwrap();

        let g = gravar(
            Destino::Neogrid,
            Some(&out),
            "id-1",
            "ACC_SELLOUT_20261022.txt",
            &BASE64.encode(b"linha\r\n"),
        )
        .unwrap();

        assert_eq!(
            g.caminho,
            out.join("ACC_SELLOUT_20261022.txt").to_string_lossy()
        );
        assert_eq!(std::fs::read(&g.caminho).unwrap(), b"linha\r\n");

        let na_out: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(na_out, vec!["ACC_SELLOUT_20261022.txt".to_string()]);

        let temporaria = raiz.join("documents").join(PASTA_TEMPORARIA);
        assert!(temporaria.is_dir());
        assert_eq!(std::fs::read_dir(&temporaria).unwrap().count(), 0);

        let _ = std::fs::remove_dir_all(&raiz);
    }

    #[test]
    fn base64_invalido_e_erro_nomeado() {
        let dir = std::env::temp_dir().join("rs-agente-b64");
        std::fs::create_dir_all(&dir).unwrap();
        let erro = gravar(Destino::Esales, Some(&dir), "id", "ACC.txt", "!!!nao-e-base64!!!").unwrap_err();
        assert!(erro.contains("base64"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grava_bytes_crus_e_confere_sha() {
        let dir = std::env::temp_dir().join("rs-agente-grava");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let latin1 = vec![b'A', 0xC7, 0xC3, b'O', b'\r', b'\n'];
        let g = gravar(Destino::Esales, Some(&dir), "id-1", "ACC.txt", &BASE64.encode(&latin1)).unwrap();

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

        gravar(Destino::Esales, Some(&dir), "id-1", "ACC.txt", &BASE64.encode(b"velho")).unwrap();
        let g = gravar(Destino::Esales, Some(&dir), "id-2", "ACC.txt", &BASE64.encode(b"novo")).unwrap();

        assert_eq!(std::fs::read(&g.caminho).unwrap(), b"novo");

        let restantes: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(restantes, vec!["ACC.txt".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
