//! Core Sega Master System do Emmu.
//!
//! O núcleo de emulação é o `smsgg-core` do JGenesis (GPL-3.0), vendorizado em
//! `vendor/jgenesis`, com o hardware forçado para `MasterSystem`. Ele cobre Z80,
//! VDP TMS9918/TMS9918A com paleta de 64 cores, PSG SN76489, opcionalmente o
//! YM2413 (FM) e os mapeadores de cartucho SMS. É Rust puro e compila para
//! WebAssembly via `wasm-pack`.
//!
//! A camada deste crate expõe a mesma API dos demais cores consumida pelo
//! worker de WebAssembly:
//!
//! - `new(rom)` / `gameTitle()`
//! - `runFrame(renderGraphics)` → RGBA8888 (256x224 NTSC ou 256x240 PAL)
//! - `audioSamples()` / `audioSampleRate()` / `audioChannels()`
//! - `setButton(button, pressed, player)` / `setVolume(volume)`
//! - `saveState()` / `loadState(bytes)` / `restart()`
//! - `saveRam()` / `loadSaveRam(bytes)` para SRAM de bateria

use bincode::{Decode, Encode};
use jgenesis_common::frontend::{
    AudioOutput, Color, ConstantInputPoller, EmulatorTrait, FrameSize, MappableInputs,
    RenderFrameOptions, Renderer, SaveWriter, TickEffect, TimingMode,
};
use jgenesis_common::input::Player;
use smsgg_config::{SmsAspectRatio, SmsGgButton, SmsGgInputs, SmsModel};
use smsgg_core::{NATIVE_Z80_DIVIDER, SmsGgEmulator, SmsGgEmulatorConfig, SmsGgHardware};
use std::num::NonZeroU32;

/// Largura padrão do frame SMS (inclui a borda esquerda de 8 px).
pub const FRAME_WIDTH: usize = 256;
/// Altura do frame SMS NTSC. ROMs PAL produzem 256x240; a UI usa a dimensão
/// devolvida pelo `runFrame`.
pub const FRAME_HEIGHT: usize = 224;
/// Frequência de saída do resampler de áudio do `smsgg-core`.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// Número de canais expostos ao frontend (estéreo intercalado).
pub const AUDIO_CHANNELS: u32 = 2;

/// Prefixo dos save states do Emmu, para rejeitar dados de outros cores.
const STATE_MAGIC: &[u8; 8] = b"EMMUSMS1";

/// Limite de ticks por frame. Um frame SMS NTSC tem ~15,7M ciclos, cada tick
/// do JGenesis processa pelo menos um ciclo; o limite serve apenas para evitar
/// loop infinito em ROMs corrompidas.
const MAX_TICKS_PER_FRAME: u32 = 32_000_000;

fn default_config() -> SmsGgEmulatorConfig {
    SmsGgEmulatorConfig {
        sms_timing_mode: TimingMode::Ntsc,
        // SMS2 é o modelo mais comum e tem a paleta definitiva. O JGenesis
        // escolhe a revisão do VDP pelo cabeçalho/cartucho quando possível.
        sms_model: SmsModel::Sms2,
        // `None` = usar a versão do PSG correta para Master System (MasterSystem2).
        forced_psg_version: None,
        sms_aspect_ratio: SmsAspectRatio::Ntsc,
        gg_aspect_ratio: Default::default(),
        remove_sprite_limit: false,
        // `None` = detectar região pelo cabeçalho/cartucho (Japão vs. internacional).
        forced_region: None,
        // Manter as bordas é importante: o MSX/SMS esconde sprites na borda e
        // vários jogos usam a área de 8 px à esquerda.
        sms_crop_vertical_border: false,
        sms_crop_left_border: false,
        gg_frame_blending: false,
        // Irrelevante para MasterSystem, mas mantido consistente com o GG.
        gg_use_sms_resolution: false,
        psg_channels_enabled: [true; 4],
        // O slot de expansão FM (YM2413) é opcional no SMS. O JGenesis só o
        // habilita quando o cartucho sinaliza suporte no cabeçalho.
        fm_sound_unit_enabled: true,
        z80_divider: NonZeroU32::new(NATIVE_Z80_DIVIDER).expect("divisor não pode ser zero"),
        allow_opposing_joypad_directions: false,
        cheat_codes: Vec::new(),
    }
}

/// SRAM de bateria. O `smsgg-core` grava via `SaveWriter`; esta implementação
/// mantém tudo em memória e expõe os bytes para o frontend persistir.
#[derive(Debug, Default)]
struct MemorySaveWriter {
    sav: Option<Vec<u8>>,
}

impl SaveWriter for MemorySaveWriter {
    type Err = String;

    fn load_bytes(&mut self, extension: &str) -> Result<Vec<u8>, Self::Err> {
        if extension == "sav" {
            Ok(self.sav.clone().unwrap_or_default())
        } else {
            Ok(Vec::new())
        }
    }

    fn persist_bytes(&mut self, extension: &str, bytes: &[u8]) -> Result<(), Self::Err> {
        if extension == "sav" {
            self.sav = Some(bytes.to_vec());
        }
        Ok(())
    }

    fn load_serialized<D: Decode<()>>(&mut self, _extension: &str) -> Result<D, Self::Err> {
        Err("SRAM serializado não é usado pelo core SMS do Emmu.".to_owned())
    }

    fn persist_serialized<E: Encode>(
        &mut self,
        _extension: &str,
        _data: E,
    ) -> Result<(), Self::Err> {
        Ok(())
    }
}

/// Coletor de áudio. O volume é aplicado aqui para que `setVolume` funcione
/// também no navegador, que consome `audioSamples()` diretamente.
#[derive(Debug)]
struct AudioSink {
    samples: Vec<f32>,
    volume: f32,
}

impl Default for AudioSink {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            volume: 1.0,
        }
    }
}

impl AudioOutput for AudioSink {
    type Err = String;

    fn push_sample(&mut self, sample_l: f64, sample_r: f64) -> Result<(), Self::Err> {
        self.samples
            .push((sample_l * f64::from(self.volume)) as f32);
        self.samples
            .push((sample_r * f64::from(self.volume)) as f32);
        Ok(())
    }
}

/// Renderer em memória: converte o framebuffer do JGenesis em RGBA8888.
#[derive(Debug)]
struct FrameRenderer {
    frame: Vec<u8>,
    width: usize,
    height: usize,
}

impl Default for FrameRenderer {
    fn default() -> Self {
        Self {
            frame: Vec::new(),
            width: FRAME_WIDTH,
            height: FRAME_HEIGHT,
        }
    }
}

impl Renderer for FrameRenderer {
    type Err = String;

    fn render_frame(
        &mut self,
        frame_buffer: &[Color],
        frame_size: FrameSize,
        _target_fps: f64,
        _options: RenderFrameOptions,
    ) -> Result<(), Self::Err> {
        self.width = frame_size.width as usize;
        self.height = frame_size.height as usize;
        let pixel_count = self.width * self.height;
        self.frame.clear();
        self.frame.reserve(pixel_count * 4);
        for color in frame_buffer.iter().take(pixel_count) {
            self.frame
                .extend_from_slice(&[color.r, color.g, color.b, color.a]);
        }
        Ok(())
    }
}

/// Core Rust puro. Usado diretamente nos testes e embrulhado pelo `SmsEmulator`
/// quando compilado para `wasm32`.
pub struct SmsCore {
    emulator: SmsGgEmulator,
    renderer: FrameRenderer,
    audio: AudioSink,
    writer: MemorySaveWriter,
    inputs: SmsGgInputs,
    rom: Box<[u8]>,
}

impl SmsCore {
    pub fn new(rom_data: &[u8]) -> Result<Self, String> {
        if rom_data.len() < 512 {
            return Err("ROM Master System inválida: arquivo muito curto.".to_owned());
        }

        let config = default_config();
        let rom = rom_data.to_vec().into_boxed_slice();
        let mut writer = MemorySaveWriter::default();
        let emulator = SmsGgEmulator::create(
            Some(rom.to_vec()),
            None,
            SmsGgHardware::MasterSystem,
            config,
            &mut writer,
        );

        Ok(Self {
            emulator,
            renderer: FrameRenderer::default(),
            audio: AudioSink::default(),
            writer,
            inputs: SmsGgInputs::default(),
            rom,
        })
    }

    pub fn game_title(&self) -> &str {
        // Cartuchos SMS não têm um campo de título padronizado no cabeçalho;
        // a interface exibe o nome do arquivo.
        "Master System"
    }

    /// Avança exatamente um frame. Com `render_graphics == false` ainda
    /// avança a emulação e o áudio, mas não devolve o framebuffer.
    pub fn run_frame(&mut self, render_graphics: bool) -> Result<Vec<u8>, String> {
        self.audio.samples.clear();

        let inputs = self.inputs;
        let mut poller = ConstantInputPoller(&inputs);
        let mut ticks = 0_u32;

        loop {
            let effect = self
                .emulator
                .tick(
                    &mut self.renderer,
                    &mut self.audio,
                    &mut poller,
                    &mut self.writer,
                )
                .map_err(|error| error.to_string())?;

            if effect == TickEffect::FrameRendered {
                break;
            }

            ticks += 1;
            if ticks >= MAX_TICKS_PER_FRAME {
                return Err(
                    "O core Master System não completou um frame dentro do limite de ticks."
                        .to_owned(),
                );
            }
        }

        if render_graphics {
            Ok(self.renderer.frame.clone())
        } else {
            Ok(Vec::new())
        }
    }

    /// Amostras estereofônicas intercaladas (L, R, L, R, ...) desde o último frame.
    pub fn take_audio_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.audio.samples)
    }

    pub fn audio_sample_rate(&self) -> u32 {
        AUDIO_SAMPLE_RATE
    }

    pub fn audio_channels(&self) -> u32 {
        AUDIO_CHANNELS
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.audio.volume = volume.clamp(0.0, 1.0) as f32;
    }

    /// Atualiza um botão. O Master System tem direções, dois botões (1/2) e
    /// Pause no-start; não tem botão Select (o do Game Gear é o Pause do SMS).
    /// O frontend mapeia A/B para os botões 1/2. X, Y, L e R são ignorados.
    pub fn set_button(&mut self, button: &str, pressed: bool, player: u8) -> Result<(), String> {
        let button = match button {
            "Up" => SmsGgButton::Up,
            "Down" => SmsGgButton::Down,
            "Left" => SmsGgButton::Left,
            "Right" => SmsGgButton::Right,
            "A" => SmsGgButton::Button1,
            "B" => SmsGgButton::Button2,
            "Start" => SmsGgButton::Pause,
            "Select" | "X" | "Y" | "L" | "R" => return Ok(()),
            _ => return Err(format!("Botão de controle SMS desconhecido: {button}")),
        };

        let player = match player {
            1 => Player::One,
            2 => Player::Two,
            _ => return Ok(()),
        };

        self.inputs.set_field(button, player, pressed);
        Ok(())
    }

    /// Reinicia com a ROM original, preservando o SRAM de bateria.
    pub fn restart(&mut self) {
        EmulatorTrait::hard_reset(&mut self.emulator, &mut self.writer);
        self.audio.samples.clear();
        let _ = self.emulator.force_render(&mut self.renderer);
    }

    /// Save state versionado: `EMMUSMS1` + estado bincode do `smsgg-core`.
    pub fn save_state(&mut self) -> Result<Vec<u8>, String> {
        let state = self.emulator.to_save_state();
        let encoded = bincode::encode_to_vec(state, bincode::config::standard())
            .map_err(|error| format!("Não foi possível salvar o estado do SMS: {error}"))?;
        let mut bytes = Vec::with_capacity(STATE_MAGIC.len() + encoded.len());
        bytes.extend_from_slice(STATE_MAGIC);
        bytes.extend_from_slice(&encoded);
        Ok(bytes)
    }

    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !bytes.starts_with(STATE_MAGIC) {
            return Err("Save state Master System inválido ou de outro console.".to_owned());
        }
        let (state, _read): (SmsGgEmulator, usize) =
            bincode::decode_from_slice(&bytes[STATE_MAGIC.len()..], bincode::config::standard())
                .map_err(|error| format!("Não foi possível carregar o estado do SMS: {error}"))?;
        EmulatorTrait::load_state(&mut self.emulator, state);
        self.audio.samples.clear();
        let _ = self.emulator.force_render(&mut self.renderer);
        Ok(())
    }

    /// SRAM de bateria bruta, para persistência fora do save state.
    pub fn save_ram(&self) -> Option<Vec<u8>> {
        self.writer.sav.clone()
    }

    pub fn load_save_ram(&mut self, bytes: &[u8]) {
        self.writer.sav = Some(bytes.to_vec());
    }

    /// ROM original carregada (usada internamente pelo restart do core).
    pub fn rom(&self) -> &[u8] {
        &self.rom
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub struct SmsEmulator {
        core: SmsCore,
    }

    #[wasm_bindgen]
    impl SmsEmulator {
        #[wasm_bindgen(constructor)]
        pub fn new(rom: &[u8]) -> Result<SmsEmulator, JsValue> {
            Ok(Self {
                core: SmsCore::new(rom).map_err(|error| JsValue::from_str(&error))?,
            })
        }

        #[wasm_bindgen(js_name = gameTitle)]
        pub fn game_title(&self) -> String {
            self.core.game_title().to_owned()
        }

        #[wasm_bindgen(js_name = runFrame)]
        pub fn run_frame(&mut self, render_graphics: bool) -> Result<Vec<u8>, JsValue> {
            self.core
                .run_frame(render_graphics)
                .map_err(|error| JsValue::from_str(&error))
        }

        #[wasm_bindgen(js_name = audioSamples)]
        pub fn audio_samples(&mut self) -> Vec<f32> {
            self.core.take_audio_samples()
        }

        #[wasm_bindgen(js_name = audioSampleRate)]
        pub fn audio_sample_rate(&self) -> u32 {
            self.core.audio_sample_rate()
        }

        #[wasm_bindgen(js_name = audioChannels)]
        pub fn audio_channels(&self) -> u32 {
            self.core.audio_channels()
        }

        #[wasm_bindgen(js_name = setButton)]
        pub fn set_button(
            &mut self,
            button: &str,
            pressed: bool,
            player: u32,
        ) -> Result<(), JsValue> {
            self.core
                .set_button(button, pressed, player as u8)
                .map_err(|error| JsValue::from_str(&error))
        }

        #[wasm_bindgen(js_name = setVolume)]
        pub fn set_volume(&mut self, volume: f64) {
            self.core.set_volume(volume);
        }

        pub fn restart(&mut self) {
            self.core.restart();
        }

        #[wasm_bindgen(js_name = saveState)]
        pub fn save_state(&mut self) -> Result<Vec<u8>, JsValue> {
            self.core
                .save_state()
                .map_err(|error| JsValue::from_str(&error))
        }

        #[wasm_bindgen(js_name = loadState)]
        pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), JsValue> {
            self.core
                .load_state(bytes)
                .map_err(|error| JsValue::from_str(&error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_rom() -> Vec<u8> {
        // ROM inócua: todos os bytes em $FF (RST $38). O JGenesis espelha o
        // arquivo até a próxima potência de dois e aceita cartuchos pequenos.
        vec![0xFF_u8; 32 * 1024]
    }

    #[test]
    fn default_config_is_master_system() {
        let config = default_config();
        assert_eq!(config.sms_timing_mode, TimingMode::Ntsc);
        assert_eq!(config.sms_model, SmsModel::Sms2);
        assert!(config.fm_sound_unit_enabled);
        assert!(!config.gg_use_sms_resolution);
        assert!(!config.sms_crop_left_border);
        assert!(!config.sms_crop_vertical_border);
    }

    #[test]
    fn runs_frame_and_returns_rgba() {
        let mut core = SmsCore::new(&minimal_rom()).expect("ROM mínima deve carregar");
        let frame = core.run_frame(true).expect("deve avançar um frame");
        assert_eq!(frame.len(), FRAME_WIDTH * FRAME_HEIGHT * 4);
        assert_eq!(frame[3], 0xFF);
    }

    #[test]
    fn psg_audio_is_not_silent() {
        let mut rom = minimal_rom();
        let code = [
            0x3E, 0x80, 0xD3, 0x7F, // ld a,$80 ; out ($7f),a  (latch Tone0)
            0x3E, 0x04, 0xD3, 0x7F, // ld a,$04 ; out ($7f),a  (Tone0 high bits)
            0x3E, 0x90, 0xD3, 0x7F, // ld a,$90 ; out ($7f),a  (Volume0 = 0)
            0x18, 0xFE, // jr $
        ];
        rom[..code.len()].copy_from_slice(&code);

        let mut core = SmsCore::new(&rom).expect("ROM com PSG deve carregar");
        let mut total_samples = 0;
        let mut peak = 0.0_f32;
        for _ in 0..180 {
            core.run_frame(true).expect("deve avançar um frame");
            for sample in core.take_audio_samples() {
                total_samples += 1;
                peak = peak.max(sample.abs());
            }
        }

        assert!(total_samples > 0, "core SMS não gerou amostras de áudio");
        assert!(peak > 0.01, "core SMS gerou apenas silêncio (pico={peak})");
    }

    #[test]
    fn save_state_round_trip() {
        let mut core = SmsCore::new(&minimal_rom()).unwrap();
        core.run_frame(true).unwrap();
        core.set_button("A", true, 1).unwrap();
        core.run_frame(true).unwrap();

        let state = core.save_state().unwrap();
        assert!(state.starts_with(STATE_MAGIC));

        core.set_button("A", false, 1).unwrap();
        core.load_state(&state).unwrap();
        assert!(core.load_state(b"ERRADO").is_err());
    }

    #[test]
    fn buttons_volume_and_restart() {
        let mut core = SmsCore::new(&minimal_rom()).unwrap();
        core.run_frame(true).unwrap();
        for button in [
            "Up", "Down", "Left", "Right", "A", "B", "Start", "Select", "X", "Y", "L", "R",
        ] {
            core.set_button(button, true, 1).unwrap();
            core.set_button(button, false, 1).unwrap();
        }
        assert!(core.set_button("Desconhecido", true, 1).is_err());
        core.set_volume(0.5);
        core.restart();
        assert_eq!(core.audio_sample_rate(), 48_000);
        assert_eq!(core.audio_channels(), 2);
        assert_eq!(core.rom().len(), 32 * 1024);
    }

    #[test]
    fn real_rom_smoke_test() {
        let path = match std::env::var("EMMU_SMS_TEST_ROM") {
            Ok(path) if !path.is_empty() => path,
            _ => return,
        };
        let rom = std::fs::read(&path).expect("ROM de teste não encontrada");
        let mut core = SmsCore::new(&rom).expect("ROM real deve carregar");
        let mut last_frame = Vec::new();
        let mut audio_samples = 0;
        for _ in 0..180 {
            last_frame = core.run_frame(true).expect("frame deve ser gerado");
            audio_samples += core.take_audio_samples().len();
        }
        let unique = last_frame
            .chunks_exact(4)
            .collect::<std::collections::HashSet<_>>();
        eprintln!(
            "SMS teste: áudio={audio_samples}, cores únicas={}, frame={}x{}, primeiros pixels={:?}",
            unique.len(),
            core.renderer.width,
            core.renderer.height,
            &last_frame[..32]
        );
        assert!(unique.len() > 1, "frame de {path} tem uma única cor");
        assert!(audio_samples > 0, "core SMS não gerou áudio para {path}");
    }
}
