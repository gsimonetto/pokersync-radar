# Auto-update — configurar os secrets do GitHub

O auto-update já está implementado no código (plugin `tauri-plugin-updater`,
chave pública em `src-tauri/tauri.conf.json`, checagem em `ui/app.js`). Falta
só um passo manual, único, pra ativar de verdade: cadastrar a chave privada
de assinatura como secret do repositório.

## Por quê

Cada versão publicada é assinada com a chave privada; o Radar instalado
confere a assinatura com a chave pública (`plugins.updater.pubkey` no
`tauri.conf.json`) antes de instalar qualquer atualização. Sem a chave
privada cadastrada, o workflow de release gera os instaladores normalmente,
mas sem os arquivos de atualização (`.sig` + `latest.json`) e com um aviso
no resumo da execução: o botão de download do site funciona, mas quem já
tem o Radar instalado não recebe aquela versão sozinho.

## Chave atual

- Par gerado em 25/09/2026 (id `8BF11A079E23FFA2`), junto com a 0.2.0 — a
  primeira versão publicada como release oficial. A chave pública já está no
  `tauri.conf.json`; a privada e a senha foram entregues ao dono do
  repositório fora do Git (nunca commitar).
- A chave anterior (id `868FA23B844862F1`, de 10/09) foi substituída: não
  estava cadastrada nos secrets (o primeiro build com atualização automática
  falhou com "Missing comment in secret key") e a única versão publicada
  antes, a v0.1.0 (pré-lançamento), é anterior à atualização automática —
  nenhum Radar instalado depende dela.

## Passo a passo

1. Vá em **Settings → Secrets and variables → Actions** neste repositório
   (`gsimonetto/pokersync-radar`).
2. Clique em **New repository secret** (ou edite, se já existir) e cadastre
   os dois:
   - `TAURI_SIGNING_PRIVATE_KEY` — o conteúdo do arquivo `.key` (uma linha
     longa em base64).
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — a senha da chave.

## Se perder a chave privada ou a senha

Gere um par novo:

```bash
npx @tauri-apps/cli@2 signer generate -w ./radar-updater.key
```

E troque `plugins.updater.pubkey` no `tauri.conf.json` pelo conteúdo do
`radar-updater.key.pub`. Atenção: os Radars já instalados só confiam na
chave antiga — não recebem sozinhos as versões assinadas com a nova; cada
jogador precisa baixar e instalar uma vez pelo site.

## Publicar uma versão nova

Suba o número da versão em `src-tauri/tauri.conf.json` e em `Cargo.toml`
(`workspace.package.version`) e rode **Actions → Release → Run workflow**
(ou faça push de uma tag `vX.Y.Z`, que já acerta a versão sozinha). O
workflow builda, assina e publica a release oficial (a "latest"); os Radars
instalados procuram versão nova ao abrir e a cada 6 horas e mostram "Nova
versão disponível".
