# 03 — Передача файлов: подключить готовый движок

**Зависимости:** `01` (грант `file_transfer` иначе невыдаваем).

## Что уже есть

`crates/net/src/file_transfer.rs` (417 строк) — законченный и покрытый тестами
движок: `ReceiveTracker` (`begin`, `apply_chunk`, `hash_chunk`, `finish`,
`cancel`, `state`), `write_chunk`/`read_chunk` поверх любой пары
`AsyncRead`/`AsyncWrite`, staging с проверкой BLAKE3 до экспорта, resume с
последнего подтверждённого смещения, лимиты `FILE_CHUNK_MAX_BYTES` и
`MAX_CONCURRENT_FILE_TRANSFERS`.

В протоколе есть `FileOffer`, `FileAccept`, `FileAbort`, `FileChunkAck`.
`ALPN_FILE` объявлен, `Channel::File` распознаётся.

## Чего нет

Проверь сам: `grep -rn file_transfer crates apps --include=*.rs` — вне самого
модуля ни одной ссылки. Соединение `rd/file/1` никогда не открывается, входящее
**принудительно закрывается** (`network.rs` ~стр. 2600), IPC-команд нет, UI нет.

## Задача 1 — Закрыть дыру в протоколе (сначала решить, потом кодить)

`MessageKind::FileAbort` и `FileChunkAck` документированы как несущие
`transfer_id`, «объявленный в `FileTransferStart`». **Такого сообщения не
существует**, а `FileOffer` идентификатора не несёт. Значит две стороны сейчас
не могут договориться о `transfer_id`.

Выбери одно и запиши решение в ADR **до** написания кода:
- (a) добавить `transfer_id` в новое сообщение `FileTransferStart`, которое
  отправитель шлёт после `FileAccept(true)`;
- (b) сделать `transfer_id` детерминированным от содержимого оффера.

Вариант (a) честнее и совпадает с тем, что уже написано в doc-комментариях.

**Правила добавления сообщения в протокол** (`crates/core/src/protocol.rs`):
- новый вариант **только в конец** `MessageKind` — postcard кодирует
  дискриминант позицией, а `tests/interop/golden_vectors.txt` заморожен;
- поднять `PROTOCOL_MINOR`;
- добавить строку возможности `FEATURE_*` рядом с `FEATURE_MEDIA_UNAVAILABLE`
  и `FEATURE_REMOTE_SAS` и слать новое сообщение **только** пиру, который её
  объявил в `Hello`;
- дописать новые golden-векторы в конец файла, существующие строки не менять;
- добавить негативные случаи в `tests/integration/tests/protocol_negative.rs`.

## Задача 2 — Ленивое соединение `rd/file/1`

**Файлы:** `apps/desktop/src-tauri/src/network.rs`, `crates/net/src/connection.rs`

- Соединение открывается **только** после `FileAccept(true)` и только для пира
  с живым грантом `file_transfer`. Это не оптимизация: §4 требует, чтобы ни
  медиапоток, ни большая передача не могли задержать revoke на control-канале.
- Входящее `Channel::File` больше не закрывается безусловно, но принимается
  **только** если для этого пира уже есть аутентифицированная control-сессия с
  грантом. Инвариант из комментария на ~стр. 2598 — «неаутентифицированный пир
  не должен парковать файловое соединение в чтении handshake» — сохраняется.
  Не удаляй этот комментарий, обнови его.
- Revoke или конец сессии закрывает файловое соединение и вызывает
  `ReceiveTracker::cancel` для всех незавершённых передач этого пира. Ничего из
  staging при этом не экспортируется.

## Задача 3 — Actor: оффер, приём, отмена, прогресс

**Файл:** `apps/desktop/src-tauri/src/network.rs`

Образец проводки — `on_monitor_select` / `on_audio_toggle`. Нужны:
`on_file_offer`, `on_file_accept`, `on_file_abort`, `on_file_progress`.

- Каждая передача — своя задача, а не работа внутри цикла actor. Смотри
  ADR 0027 (`docs/adr/0027-the-dial-leaves-the-actor-loop.md`): блокирующая
  работа в цикле actor уже один раз уронила картинку, повторять не надо.
- Прогресс отдаётся в UI через `ActorNotification`, как `ClipboardFromPeer`.
- Путь назначения выбирает **принимающий пользователь**, а не отправитель.
  Имя из `FileOffer` нормализуется до basename без разделителей пути —
  проверь, что нормализация действительно есть, и добавь тест на
  `../../etc/passwd` и на Windows-варианты (`..\`, `C:`, ADS `name:stream`).
- В аудит — `AuditEvent::FileAction { action }`. **Имя файла в аудит не
  попадает** (§15), только тег действия.

## Задача 4 — IPC, capability, UI

**Файлы:** `commands.rs`, `main.rs`, `capabilities/*.json`, `apps/desktop/src/`

- Команды: `file_offer`, `file_accept`, `file_abort`, `file_transfers`
  (список с прогрессом для отрисовки).
- Диалог выбора файла: **не** давать webview право `dialog`/`fs` целиком.
  Открывай нативный диалог со стороны Rust и передавай в webview только
  метаданные. Описание `capabilities/view.json` прямо говорит, что окно
  просмотра не имеет прав на файловую систему — не ослабляй это.
- UI: список передач с именем, размером, процентом и кнопкой отмены;
  входящий оффер требует явного подтверждения. Ключи i18n в `en` и `ar`.

## Задача 5 — Интеграционный тест

Новый `tests/integration/tests/file_transfer.rs`:
- полный цикл offer → accept → чанки → проверка хеша → экспорт;
- отказ `FileAccept(false)` — соединение не открывается вовсе;
- отмена посередине — из staging ничего не появилось на диске;
- resume после разрыва внутри `RECONNECT_WINDOW_SECS` продолжает с последнего
  `FileChunkAck`, а не с нуля;
- оффер без гранта `file_transfer` отклоняется.

## Проверка

```sh
cargo test --workspace && cargo test -p lumepeer-integration-tests --test file_transfer && cargo test -p lumepeer-integration-tests --test protocol_golden
```

## Ловушки

- `FILE_OFFER_MAX_BYTES` = 500 MiB, `FILE_CHUNK_MAX_BYTES` = 256 KiB,
  `MAX_PENDING_FILE_OFFERS` = 3, `MAX_CONCURRENT_FILE_TRANSFERS` = 3. Все уже
  есть в `constants.rs` — брать оттуда, не вводить свои.
- Длина чанка проверяется **до** аллокации. Движок это уже делает; не обходи
  его, читая напрямую в `Vec::with_capacity(announced_len)`.
- `crates/net` — часть TCB: `#![forbid(unsafe_code)]`, никаких
  `unwrap`/`expect` на пути разбора.
