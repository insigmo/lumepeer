# 31 — Подключение только к терминалу

**Пункт бэклога:** нет — задача пришла отдельно, вне `docs/gap-backlog.md`.
**Зависит от:** `17` (удалённый терминал; здесь он не переделывается, только
используется). **Нужно:** две машины для ручной проверки.

Терминал уже есть: канал `rd/term/1`, грант `terminal`, панель в окне
просмотра (ADR 0079). Чего нет — способа подключиться к хосту **только** ради
оболочки. Сейчас «Connect again» всегда поднимает картинку, и сессия, в
которой человек хотел выполнить одну команду, стоит хосту захвата, энкодера,
потока и декодера.

Аналог `ssh host`, но по существующему транспорту Lumepeer и существующему
каналу. Ничего в протоколе не меняется.

## Что уже есть (проверено по коду на v0.0.81)

- Меню карточки сохранённого хоста — `apps/desktop/src/session-status.ts`,
  блок `history.map(...)`, массив пунктов в `peerMenu(...)`. Роль строки —
  `entry.role`.
- Повторный коннект: `onReconnect` → `invoke('history_connect')` →
  `commands.rs::history_connect` → `ActorCommand::HistoryConnect` →
  `Actor::spawn_dial` → `spawn_dial_as` (`crates/runtime/src/network.rs`).
- Гость поднимает сессию в `Actor::start_view`: `spawn_media_receiver`,
  `ViewFeed`, `ViewState`, `self.windows.open(...)`.
- Окно — `apps/desktop/src-tauri/src/view_windows.rs::open`, URL
  `view.html?peer=..&host=..&input=0|1`, метка `view-{peer}`.
- Фронт окна — `apps/desktop/src/view.ts`, разметка `apps/desktop/view.html`
  (`#terminal-panel` / `#terminal-chrome` / `#terminal-screen`), эмулятор
  монтируется лениво через `mountTerminal`.
- Энкодер на хосте стартует **только** при принятой медиа-связи
  (`Actor::on_media_accepted`), и кадр из `CaptureController` читает только
  цикл кодирования. Гость, не открывший `rd/media/1`, не заставляет хост
  ничего кодировать.

**Расхождение с исходным описанием, найденное по коду.** Захват на хосте
стартует **не** при медиа-связи, а при выдаче гранта:
`start_granted_session` зовёт `CaptureController::add_viewer`, а тот —
`capturer.start(target)`. То есть «на хосте нет захвата экрана» в буквальном
смысле недостижимо без сообщения в протоколе. Достижимо и достигнуто другое:
ни энкодера, ни единого прочитанного кадра, ни байта картинки на проводе.
Подробности и почему решено именно так — ADR 0101, раздел «Consequences».

## Задача 1 — ADR

`docs/adr/0101-a-terminal-session-is-the-same-session-with-no-media-connection.md`.

Отвечает ровно на один вопрос: терминальная сессия — это та же сессия и та же
роль, просто гость не открывает медиа-канал, поэтому хост ничего не кодирует.
Следствие: на хосте без бэкенда захвата терминал работает, а картинка нет.
Номер взят по правилу из `README.md` этой папки (грепом по ссылкам, не по
листингу).

## Задача 2 — флаг сквозь коннект

**Файлы:** `crates/runtime/src/network.rs`,
`apps/desktop/src-tauri/src/commands.rs`

1. `HistoryConnectArgs` получает `terminal_only: bool` (`#[serde(default)]`),
   `ActorCommand::HistoryConnect` — поле `terminal_only`.
2. `spawn_dial_as` получает третий параметр и кладёт `addr.id` в новое поле
   актора `pending_terminal_only: HashSet<NodeId>` — по образцу
   `pending_remember` / `connect_credentials_auto`.
3. `start_view` забирает запись (`HashSet::remove`) и при флаге не зовёт
   `spawn_media_receiver`: `ViewState.task` становится `Option<JoinHandle<()>>`
   и равен `None`. `ViewState`/`ViewFeed` остаются — на них висят метка,
   гранты и снапшот; пустой слот кадров допустим.
4. Окно открывается с `input=false`, чтобы не вешать клавиатурный перехват
   (`watch_focus_for_the_keyboard_grab`, ADR 0090).
5. Чистят флаг: `stop_view`, `on_connect_cancel` и `settle_connect` с любым
   исходом кроме `Connected`.
6. Переносят флаг там, где продолжается **тот же** коннект: раунды ADR 0096
   (`on_connect_retry_tick`) и возобновление ADR 0089 — для второго флаг едет
   на `ReconnectWait`, потому что `stop_view` к этому моменту уже отработал.
   Без этого сессия вернулась бы картиночной, и хост начал бы кодировать.
7. Метка окна остаётся `view-{peer}`: `check_view_window` не трогать.

## Задача 3 — окно

**Файлы:** `apps/desktop/src-tauri/src/view_windows.rs`,
`apps/desktop/src/view.ts`, `apps/desktop/view.html`,
`apps/desktop/src/toolbar.ts`

1. `ViewWindows::open` получает `terminal_only: bool` (правятся все четыре
   реализации трейта), кладёт его в URL как `&terminal=1` и берёт для такого
   окна заголовок про терминал.
2. `view.ts` при `terminal=1`: не запускает ни один цикл кадров, ни опрос
   курсора, ни миниатюры, ни панорамирование, ни оверлей статуса (иначе он
   навсегда останется на «ждём картинку»); прячет `#screen` и `#cursor`;
   показывает `#terminal-panel` во всё окно и сразу монтирует терминал.
3. Тулбар показывает только осмысленное: терминал, файловый менеджер по
   гранту, чат, свернуть. Уходят настройки, мониторы, масштаб, микрофон,
   Ctrl+Alt+Del, запрос записи и полноэкранный режим — они все про картинку
   или про медиа-связь. Не «disabled», а отсутствуют (§18).
4. Закрытие окна по-прежнему `session_revoke`.

## Задача 4 — интерфейс карточки и локали

**Файлы:** `apps/desktop/src/session-status.ts`,
`apps/desktop/src/invite-view.ts`, `apps/desktop/src/main.ts`,
`apps/desktop/src/i18n.ts`, `apps/desktop/src/locales/*.ts`

1. Пункт «Connect to terminal» в `peerMenu` рядом с «Connect again».
2. Он disabled для строки, чья роль не `full_control`: грант `terminal` даёт
   только `Role::FullControl` (`Grants::from_role`), роль приходит из кода
   приглашения, гость её не выбирает. У кнопки есть `title` с причиной.
3. Два новых ключа (`status.reconnectTerminal` и
   `status.reconnectTerminal.needsFullControl`) во всех 13 локалях.

## Чего НЕ делать

- Не добавлять вариант в `Role`: он едет в `Hello`/`ConsentGrant` через
  postcard, новый вариант ломает разбор у старых пиров.
- Не добавлять грант, ALPN, сообщение протокола, не трогать
  `PROTOCOL_MINOR`.
- Не заводить окно с другой меткой и не ослаблять `check_view_window`.
- Не трогать `crates/terminal`, `crates/net/src/terminal.rs` и протокол
  терминала.
- Не давать гостю выбирать исполняемый файл оболочки (ADR 0079).

## Definition of done

- `cargo test --workspace` — не хуже базового прогона; новый тест актора:
  коннект с `terminal_only` не создаёт медиа-задачу и открывает окно с
  `input=false`.
- `cargo clippy --workspace --all-targets -- -D warnings` — чисто.
- `cd apps/desktop && npm run typecheck && npm test` — зелёное; новый тест
  `session-status`: пункт меню есть и он disabled для роли `view_only`.
- Ручная проверка на паре машин: «Connect to terminal» открывает окно только
  с терминалом; на хосте виден индикатор терминала; в логе хоста нет строки
  `media connection accepted; starting the encode loop`; отзыв гранта
  `terminal` закрывает оболочку немедленно; закрытие окна завершает сессию.
