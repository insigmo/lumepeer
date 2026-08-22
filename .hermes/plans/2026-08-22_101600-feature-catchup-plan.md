# План догоняния функциональности lumepeer до уровня TeamViewer/AnyDesk/RustDesk

> Дата: 2026-08-22. Основан на аудите репозитория против списка отсутствующих
> функций. Часть пунктов исходного списка уже реализована — см. раздел
> «Что уже есть».

## Что уже есть (не переделывать)

- **Windows**: DXGI Desktop Duplication захват (`capture-windows`, ADR 0012),
  Media Foundation hardware H.264 encoder (`encode-mf`, ADR 0011).
- **macOS**: ScreenCaptureKit захват + CGEvent инжектор ввода + TCC-обработка
  разрешений (`capture-screencapturekit`, ADR 0013). Полностью реализован.
- **Wayland**: портал-переговоры (CreateSession→SelectDevices→SelectSources→Start)
  сделаны (`capture-portal`); потребление PipeWire-кадров — нет (честная ошибка).
- **Ядро**: протокол §9.1 несёт ClipboardSync, FileOffer/FileAccept,
  RecordRequest/Ack, QualityAdjust; consent/grants полные; audit.rs есть.
- **Сеть**: три ALPN-соединения, reconnect-window, keystore (in-memory/
  encrypted-file/OS-native), тикеты+QR, анти-replay.
- **Десктоп**: Tauri UI с view-окном, consent-dialog, invite-view, session-status,
  i18n en/ar, connection history.

## Этап 0 — аудит и вопросы ✅

- Сверка списка с кодом (этот файл).
- `questions.md` в корне — все открытые вопросы туда.

## Этап 1 — кроссплатформенный фундамент (тестируемо на любой машине)

Все изменения тестируемы `cargo test --workspace` без платформенных SDK.

1. **Протокол+константы**: новые `MessageKind`: `Chat{text}`, `KeyframeRequest`,
   `CursorShape{...}`, `MonitorsList/MonitorSelect`,
   `PrivacyMode(bool)/PrivacyModeAck`, `AudioStart/AudioStop`,
   `StatsReport{...}`; константы: `CHAT_MAX_BYTES`, `AUDIO_*`,
   `FILE_CHUNK_*`, `WOL_*` — всё через constants.rs.
2. **core/unattended.rs**: постоянный пароль устройства (argon2), per-NodeID
   rate-limit неудачных попыток, TOTP-2FA (RFC 6238, свои ~100 строк с тестами
   на RFC-векторах), snapshotted при гранте как остальная политика.
3. **core/address_book.rs**: сохранённые устройства (NodeId→метка/теги/заметки),
   trusted-device whitelist, сериализация в JSON рядом с config.
4. **net/file_transfer.rs**: реальный менеджер поверх rd/file/1: очередь передач,
   чанки с length-prefix, контрольные суммы blake3, resume по offset, отмена,
   прогресс-колбеки. Интеграционные тесты через in-memory duplex.
5. **net/stats.rs**: RTT/jitter/loss по Ping/Pong, direct vs relay из
   `ConnectionType` iroh, структура `ConnStats` для UI.
6. **net/wol.rs**: Wake-on-LAN magic packet (6×0xFF + 16×MAC), UDP broadcast.

## Этап 2 — Windows-хост (машинка разработки)

1. **Инжектор SendInput**: клавиатура (scancode+unicode) + мышь absolute/relative
   + колесо. За фичей `inject-windows`.
2. **Буфер обмена**: текстовый sync хост↔гость через `ClipboardSync` (Win32 API).
3. **Аудио**: WASAPI loopback capture + opus encode → новый media-канал;
   воспроизведение у гостя.
4. **Privacy mode**: черный экран + блокировка локального ввода во время сессии.
5. **Мульти-монитор**: перечисление мониторов DXGI, выбор цели захвата.

## Этап 3 — UI (TS webview)

Чат-панель, файловый менеджер, адресная книга, панель статистики соединения,
выбор монитора/качества/FPS, полноэкранный режим, тёмная тема. i18n en/ar для
всего нового. axe-core тесты.

## Этап 4 — Linux / macOS доделки

PipeWire-потребление кадров (capture-portal), uinput/libei ввод Wayland,
VAAPI энкодер, PulseAudio/PipeWire audio; macOS: CoreAudio, VideoToolbox probe.

## Этап 5 — Инфраструктура

Relay deployment doc + docker-compose (self-host iroh-relay), unattended
service-режим (автозапуск до логина), автообновления (updater server contract),
session recording writer, ADR на каждое отклонение от спеки.

## Верификация каждого этапа

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/desktop && npm run typecheck && npm test
```
