# 11 — Довезти Wayland и Linux-аудио до релиза

**Зависимости:** нет. Нужна Linux-машина с Wayland-сессией для проверки —
эмулировать это в CI полноценно нельзя.

## Суть задачи

Wayland-путь **написан целиком**: `crates/media/src/capture/linux_wayland.rs`
(682 строки) ведёт переговоры с xdg-desktop-portal, `capture/pipewire_stream.rs`
(327 строк) потребляет PipeWire-поток, инъекция идёт через `RemoteDesktop`
(`notify_pointer_motion_absolute`, `notify_pointer_axis`,
`notify_pointer_button`, `notify_keyboard_keycode`), а `platform_backend()`
отдаёт захват и инжектор одной парой, потому что `notify_*` требуют того же
`Session`, на котором прошёл `Start` (ADR 0010, ADR 0025).

**Но в собранный продукт этот код не попадает.** Проверь сам:

```sh
grep -n MEDIA_FEATURES Taskfile.yml            # LINUX_MEDIA_FEATURES: capture-x11,encode-openh264
grep -n 'features: capture' .github/workflows/release.yml
```

Фичи `capture-portal` там нет. Аналогично `audio-capture-pipewire` не включена
нигде — Linux-хост не отдаёт звук рабочего стола, а `playout::platform_player`
реализован **только для Windows**, так что Linux-гость и микрофон не услышит.

## Задача 1 — Проверить Wayland-путь на живой машине

Сначала проверка, потом изменение сборки. Собрать локально:

```sh
cd apps/desktop && npm install && npm run build && cd -
cargo build -p lumepeer-desktop --features capture-portal,capture-x11,encode-openh264
```

Проверить на Wayland-сессии: появляется диалог портала; после согласия идёт
картинка; инъекция мыши и клавиатуры доходит; отзыв согласия останавливает
захват; закрытие сессии портала обрабатывается без паники.

Проверить, что на X11-сессии тот же бинарник по-прежнему идёт по X11-пути:
`detect_session_type()` (`capture/mod.rs`) выбирает портал только при
`Wayland` или `Unknown`, и `Unknown` намеренно уходит в портал.

Записать результат — что заработало, что нет — в ADR. Если что-то не работает,
дальше идут задачи по починке, а не по включению в релиз.

## Задача 2 — Включить `capture-portal` в сборку

**Файлы:** `Taskfile.yml` (стр. 13), `.github/workflows/release.yml` (~стр. 164,
168)

- Добавить `capture-portal` в `LINUX_MEDIA_FEATURES` и в обе Linux-строки
  матрицы релиза (amd64 и arm64).
- `ashpd` и `pipewire` тянут системные зависимости. Проверить, что раннер
  релиза их ставит, а `.deb`/`.rpm` объявляют их в зависимостях пакета — иначе
  установка пройдёт, а приложение упадёт при первом захвате.
- README перечисляет системные пакеты для сборки на Linux. Обновить список.
- Проверить, что `capture-x11` остаётся: один бинарник обязан уметь обе
  сессии, выбор делается в рантайме.

## Задача 3 — Аудио на Linux

**Файлы:** `apps/desktop/src-tauri/Cargo.toml`, `Taskfile.yml`,
`release.yml`, `crates/media/src/playout.rs`

- Включить `audio-capture-pipewire` для Linux-целей. Сейчас в
  `apps/desktop/src-tauri/Cargo.toml` в общей секции стоит
  `lumepeer-media = { features = ["audio-opus", "audio-capture"] }`, где
  `audio-capture` — это `windows/Win32_Media_Audio`, то есть на Linux она не
  делает ничего. Добавить фичу в Linux-таблицу целей рядом с `capture-x11`.
- `playout.rs` реализован только под Windows: заголовок модуля честно это
  говорит, `platform_player()` на остальных платформах отказывает. Добавить
  Linux-бэкенд воспроизведения (PipeWire-поток на вывод), сохранив ту же
  форму: `start` / `push` (один чанк проводного формата) / `stop`.
- Конвертация в проводной формат и обратно уже есть — `to_wire_pcm` в
  `capture_audio.rs` и обратная в `playout.rs`. **Переиспользовать, не
  писать вторую.** Проводные параметры фиксированы константами
  `AUDIO_SAMPLE_RATE_HZ`, `AUDIO_CHANNELS`, `AUDIO_FRAME_MS` — ничего не
  согласовывать на сессию.

## Задача 4 — Перечисление мониторов на Linux

**Файл:** `crates/media/src/capture/mod.rs`, `host_monitors()`

Сейчас функция реализована **только для Windows**, а на всех остальных
платформах возвращает один фиктивный монитор с `width: 0, height: 0`. В
комментарии там честно написано, почему так, и что это временно.

- X11: перечислить выходы через RandR (`x11rb`).
- Wayland: взять то, что отдаёт портал при выборе источника; если портал
  отдаёт один поток без списка — вернуть одну запись с **реальными**
  размерами, а не нулями.
- Соблюдать `MAX_MONITORS_PER_HOST = 8` при формировании `MonitorsList`.
- `CaptureTarget::Display(u32)` должен индексировать тот же порядок, что и
  возвращает `host_monitors()` — это контракт, и его надо закрыть тестом.

## Задача 5 — CI

**Файл:** `.github/workflows/ci.yml`

- Джоба `media` уже гоняет `cargo clippy -p lumepeer-media --features
  capture-portal` под dbus. Добавить туда сборку и clippy с новым набором
  Linux-фич целиком, включая аудио.
- Тесты, требующие живого дисплея или звукового сервера, должны быть за
  переменной окружения, как это сделано для XTEST
  (`LUMEPEER_TEST_XTEST=1`) — не включать их в обычный прогон.

## Проверка

```sh
cargo clippy -p lumepeer-media --all-targets --features capture-portal,capture-x11,encode-openh264,audio-opus,audio-capture-pipewire -- -D warnings
cargo test --workspace
```

## Ловушки

- Портал показывает **свой** диалог выбора источника. Это не обходится и не
  должно обходиться — `CLAUDE.md`: «no bypassing OS permission prompts».
- Инжектор Wayland нельзя строить отдельно от захвата: получится вторая
  сессия портала, второй диалог и инъекция в то, что никто не захватывает.
  `platform_backend()` возвращает пару именно поэтому — не «упрощай» это.
- Пользователь может закрыть сессию портала в любой момент. Это штатный
  исход: остановка захвата и сообщение, а не паника.
- ADR 0003 откладывал Wayland намеренно. Его закрытие — это новый ADR,
  ссылающийся на 0003 и 0010, а не правка старого.
