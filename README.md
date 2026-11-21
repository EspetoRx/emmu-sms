# emmu-sms

Core de **Sega Master System** usado pelo Emmu no navegador. No desktop, o mesmo
console é executado pelo MesenCE através de `emmu-mesen-sys`; este crate não é
carregado pela aplicação Tauri.

## O que está incluído

- Wrapper Rust/WASM com a API esperada pelo worker da interface:
  `new(rom)`, `gameTitle()`, `runFrame(renderGraphics)`, `audioSamples()`,
  `audioSampleRate()`, `audioChannels()`, `setButton()`, `setVolume()`,
  `restart()`, `saveState()` e `loadState()`.
- Frame RGBA8888 nativo de **256×224** (NTSC) ou **256×240** (PAL), com as
  bordas do VDP preservadas.
- Áudio estereofônico a **48 kHz**; o volume é aplicado antes de devolver as
  amostras ao `AudioWorklet`.
- Save states com magic `EMMUSMS1` seguido do estado serializado pelo
  `smsgg-core`. Estados com outro magic são rejeitados.
- SRAM de bateria mantida em memória e preservada no `restart()`.

## Dependência de emulação

Todo o código de emulação fica dentro desta pasta, em `vendor/jgenesis`:

- `backend/smsgg-core` — Z80, VDP, mapeadores e núcleo SMS/GG;
- `backend/ti-sn76489` — PSG;
- `backend/ym-opll` — expansão FM YM2413 do SMS;
- `cpu/z80-emu`, `common/*`, `config/smsgg-config` e dependências auxiliares.

O upstream é [`jsgroth/jgenesis`](https://github.com/jsgroth/jgenesis), commit
`8d1bdfba460b3a49b493aa00368926ea375d6074`, GPL-3.0-only. Detalhes e
modificações locais estão em `UPSTREAM.txt`; a licença está em `LICENSE` e em
`vendor/jgenesis/LICENSE`.

## Configuração

`default_config()` força:

- `SmsGgHardware::MasterSystem` (sem BIOS);
- modelo SMS2, região detectada automaticamente pelo cartucho;
- PSG SN76489 versão MasterSystem2 e expansão FM disponível;
- bordas verticais e esquerda preservadas (8 px de overscan horizontal);
- aspecto NTSC e blending de frame desligado.

## Controles

| Ação do Emmu | Botão Master System |
| --- | --- |
| Direções | D-pad |
| `A` / `B` | Botão `1` / `2` |
| `Start` | `Pause` |
| `Select`, `X`, `Y`, `L`, `R` | ignorados (não existem no hardware) |

O Master System tem suporte a 2 jogadores no core; a UI expõe P2 como nos
outros consoles de controle.

## Build

```sh
cd platforms/web/ui
npm run build:wasm
```

Saída: `platforms/web/ui/src/wasm/sms/emmu_sms.js`.

## Testes

```sh
cargo test -p emmu-sms
```

Com uma ROM de jogo válida (não BIOS):

```sh
EMMU_SMS_TEST_ROM="/caminho/jogo.sms" cargo test -p emmu-sms -- --nocapture
```
