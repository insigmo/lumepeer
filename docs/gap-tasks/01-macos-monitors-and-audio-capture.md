# 01 — macOS: перечисление мониторов и захват звука

**Пункты бэклога:** 7, 5. **Зависит от:** ничего. **Нужно:** живой Mac
(x86_64 или arm64) с разрешениями Screen Recording и Microphone.

Дополняет `docs/tasks/12-macos-completion.md` — там та же работа описана
подробнее по шагам ScreenCaptureKit; этот файл её не отменяет, читать оба.

## Что уже есть (проверено по коду)

- `crates/media/src/capture/macos.rs` — захват через ScreenCaptureKit,
  `SCShareableContent`, делегат `SCStreamOutput` на dispatch-очереди.
  `select_display` (~стр. 488) уже ходит по настоящему списку
  `content.displays()` и умеет `CaptureTarget::PrimaryDisplay` и
  `Display(n)`.
- `crates/media/src/capture/mod.rs`, `host_monitors()` (~стр. 79): на macOS
  попадает в ветку-заглушку, возвращающую один `HostMonitor { id: 0, width:
  0, height: 0, primary: true }`, с `TODO(docs/tasks/12-macos-completion.md)`.
- `crates/media/src/capture/macos.rs` ~стр. 615: `config.setCapturesAudio(false)`
  — ScreenCaptureKit готов отдавать звук, его выключили.
- `crates/media/src/capture_audio.rs`: трейт `AudioCapturer`, тип `PcmChunk`
  (48 кГц, стерео, s16, чанк `AUDIO_FRAME_MS`), конвертер `to_wire_pcm`,
  и `platform_audio_capturer()` (~стр. 175), который на macOS возвращает
  `MediaError::CaptureUnavailable`.
- Фичи media объявлены в `crates/media/Cargo.toml`: `capture-screencapturekit`,
  `audio-capture` (WASAPI, Windows), `audio-capture-pipewire` (Linux).
- Клиентская сборка включает фичи в `apps/desktop/src-tauri/Cargo.toml`:
  общая секция даёт `audio-opus` и `audio-capture`, а
  `[target.'cfg(target_os = "linux")'.dependencies]` добавляет
  `audio-capture-pipewire`. Для macOS такой строки нет.
- Релизная матрица: `.github/workflows/release.yml`, строки `macos-arm64` и
  `macos-amd64`, `features: capture-screencapturekit,encode-openh264`.

## Задача 1 — `host_monitors()` на macOS

**Файлы:** `crates/media/src/capture/mod.rs`, `crates/media/src/capture/macos.rs`

1. В `macos.rs` добавь публичную функцию перечисления по образцу
   `windows::WindowsCapturer::attached_monitors_info()`: возьми
   `SCShareableContent`, пройди `content.displays()` **в том же порядке**, в
   каком по нему ходит `select_display`, и собери `Vec<HostMonitor>`.
   Порядок — это контракт: `id` здесь и индекс `CaptureTarget::Display(n)`
   обязаны совпадать, иначе гость выберет не тот экран.
2. `primary` — тот дисплей, который система считает главным (у
   `CGMainDisplayID` тот же `displayID`, что у `SCDisplay`). Если определить
   не удалось — `primary: true` у первого и ни у кого больше, но не у всех.
3. В `mod.rs` замени macOS-ветку заглушки на вызов новой функции. Заглушка
   остаётся только для «нет бэкенда вообще» (сборка без
   `capture-screencapturekit`) — там по-прежнему честный
   `MediaError::CaptureUnavailable`, а не фиктивный монитор.
4. Удали `TODO`, который перестал быть правдой.

## Задача 2 — захват звука рабочего стола

**Файлы:** `crates/media/src/capture/macos.rs`,
`crates/media/src/capture_audio.rs`, `crates/media/Cargo.toml`,
`apps/desktop/src-tauri/Cargo.toml`, `.github/workflows/release.yml`

1. Новая фича `audio-capture-screencapturekit` в `crates/media/Cargo.toml` по
   образцу соседних: инертна на других платформах, `cargo build --workspace`
   без неё не требует SDK.
2. Под этой фичей включи `setCapturesAudio(true)` и забирай аудио-семплбуферы
   из того же `SCStream` — делегат уже различает `SCStreamOutputType`, ветка
   `Screen` есть, нужна ветка `Audio`.
3. Реализуй `AudioCapturer` поверх этого потока и подключи его в
   `platform_audio_capturer()` новой `cfg`-веткой. Формат наружу — ровно тот
   же `PcmChunk`. **Конвертацию делать существующей `to_wire_pcm`** — второй
   ресемплер не писать.
4. Захват звука не стартует без активного зрителя и без гранта `view` — то же
   правило, что для пикселей (§8.1). Точка старта та же, что у видео.
5. Отказ в разрешении на запись звука — честный `MediaError`, видеосессия
   продолжается без звука (§18), не паника и не тишина молча.
6. Пропиши фичу в клиентскую сборку: секция
   `[target.'cfg(target_os = "macos")'.dependencies]` в
   `apps/desktop/src-tauri/Cargo.toml` получает
   `lumepeer-media = { workspace = true, features = ["audio-capture-screencapturekit"] }`
   — ровно как Linux получает `audio-capture-pipewire`. Плюс проброс-фича в
   секции `[features]` того же файла, как у соседей.
7. Добавь фичу в обе macOS-строки релизной матрицы.

## Чего НЕ делать

- Не ставить виртуальное устройство вывода и не тянуть CoreAudio-loopback:
  ScreenCaptureKit отдаёт микс сам, это и есть выбранный путь.
- Не менять формат провода (48 кГц/стерео/s16/`AUDIO_FRAME_MS`) и не трогать
  константы `AUDIO_*`.
- Не трогать Windows- и Linux-бэкенды и не «унифицировать» их заодно.
- Не включать новую фичу в `default` ни в одном крейте.
- Не выдумывать порядок мониторов «поудобнее»: он обязан совпасть с
  `select_display`.
- Не браться здесь за воспроизведение (`playout.rs`) — это пачка `02`.

## Definition of done

- `cargo clippy -p lumepeer-media --all-targets --features capture-screencapturekit,encode-openh264 -- -D warnings` — чисто.
- То же с добавленной `audio-capture-screencapturekit` — чисто.
- `cargo build --workspace` **без** платформенных фич по-прежнему собирается.
- На живом Mac: `host_monitors()` возвращает столько записей, сколько
  физически подключено, с непулевыми размерами; выбор `Display(1)` в госте
  показывает второй экран.
- На живом Mac: гость слышит звук, который играет на хосте; отзыв гранта
  `view` останавливает и звук тоже.
- `cargo test --workspace` не хуже базового прогона.
