# 03 — macOS: аппаратный энкодер VideoToolbox

**Пункт бэклога:** 8. **Зависит от:** ничего. **Нужно:** живой Mac.
Смежный документ первой волны — `docs/tasks/13-hardware-encoders.md`.

## Что уже есть (проверено по коду)

- `crates/media/src/encode/mod.rs` — трейт `VideoEncoder` (`encode`,
  `set_bitrate`, `request_keyframe`, `kind`), `EncoderConfig`
  (`fps`, `bitrate_kbps`, `codec`), `EncodedFrame`
  (`keyframe`, `timestamp_us`, `data`).
- `probe_hardware(config) -> Option<EncoderKind>`: **сначала** отказывает
  всему, кроме `VideoCodec::H264`, потом идёт по платформам. На macOS сейчас
  `None` с комментарием «VideoToolbox и MediaCodec — phase 4».
- `select_encoder` выбирает аппаратный, если проба сказала да, иначе
  `openh264` (`EncoderKind::SoftwareOpenH264`).
- Образцы для копирования: `crates/media/src/encode/windows.rs` (Media
  Foundation, ADR 0011) и `crates/media/src/encode/linux_vaapi.rs` (VA-API,
  ADR 0040). Оба **пробуют по-настоящему**: активируют кодировщик, скармливают
  ему NV12 и только тогда сообщают `Hardware`.
- Конвертация в NV12 уже есть: `crates/media/src/encode/nv12.rs`.
- Релизная матрица macOS: `features: capture-screencapturekit,encode-openh264`.

## Задача 1 — бэкенд

**Файлы:** `crates/media/src/encode/macos_videotoolbox.rs` (новый),
`crates/media/src/encode/mod.rs`, `crates/media/Cargo.toml`

1. Новая фича `encode-videotoolbox` по образцу `encode-mf`/`encode-vaapi`:
   инертна на других платформах, не в `default`, зависимости объявлены
   опционально в macOS-таблице целей.
2. Реализуй `VideoEncoder` через `VTCompressionSession`:
   `kVTCompressionPropertyKey_RealTime = true`,
   `AllowFrameReordering = false` (B-кадры дают задержку, которой в
   интерактивной сессии быть не должно), `ProfileLevel` — Baseline/Main под
   H.264, `AverageBitRate` из `EncoderConfig`.
3. `request_keyframe` — `kVTEncodeFrameOptionKey_ForceKeyFrame` на следующем
   кадре. Бюджет `KEYFRAME_MIN_INTERVAL_MS` соблюдает вызывающий, не бэкенд.
4. `set_bitrate` — переустановка свойства на живой сессии, без пересоздания.
5. На выходе — Annex-B поток: гость (`apps/desktop/src/view-decoder.ts`)
   выводит строку `avc1.PPCCLL` из самого потока и ждёт Annex-B. VideoToolbox
   по умолчанию отдаёт AVCC (length-prefixed) — конвертацию делать здесь, в
   бэкенде, и покрыть тестом.
6. `kind()` возвращает `EncoderKind::Hardware`.

## Задача 2 — честная проба

**Файл:** `crates/media/src/encode/mod.rs`

Добавь macOS-ветку в `probe_hardware`, которая действительно создаёт
`VTCompressionSession` и кодирует один кадр, и только тогда отвечает
`Some(Hardware)`. Ответ «да» без реальной активации — тот самый дефект,
из-за которого v0.0.14 показывала пустой экран (см. комментарий в
`.github/workflows/release.yml` над матрицей).

`VideoCodec::Av1` здесь по-прежнему `None` — AV1 это пачка `07`, и отвечать
за него H.264-ответом нельзя.

## Задача 3 — релиз

Добавь `encode-videotoolbox` в обе macOS-строки матрицы
`.github/workflows/release.yml` и в проброс-фичи
`apps/desktop/src-tauri/Cargo.toml`. `encode-openh264` из строк **не
убирать**: программный путь остаётся резервом (§18).

## Чего НЕ делать

- Не убирать openh264 с macOS и не делать VideoToolbox обязательным.
- Не менять трейт `VideoEncoder` и сигнатуру `probe_hardware`.
- Не трогать Windows/VA-API бэкенды «заодно».
- Не включать B-кадры и не «повышать качество» ценой задержки: ADR 0059 —
  хост кодирует ради задержки.
- Не браться за AV1/H.265 (пачки `07`, `08`).

## Definition of done

- `cargo clippy -p lumepeer-media --all-targets --features capture-screencapturekit,encode-videotoolbox,encode-openh264 -- -D warnings` — чисто.
- `cargo build --workspace` без фич собирается.
- `cargo test -p lumepeer-media --features encode-videotoolbox` зелёный,
  включая тест AVCC→Annex-B.
- На живом Mac: сессия идёт, в статусе видно `Hardware`, картинка появляется
  и не рассыпается после 10 минут (ключевые кадры действительно приходят).
- На Mac без поддержки (или при принудительно сломанной пробе) сессия
  продолжается на openh264 — проверить, что деградация работает.
