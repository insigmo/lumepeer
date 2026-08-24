# Progress: LumePeer через интернет (хост `beta`, отладка по SSH/Tailscale)

Дата: 2026-08-24. Задача: соединить два LumePeer-клиента напрямую через интернет
(Iroh/QUIC), SSH поверх Tailscale — только как канал управления удалённой машиной.

---

## ✅ Успел сделать

### Инфраструктура и деплой
- **SSH до `bberb@beta.tail47af6.ts.net` работает** (BatchMode, ключи). Удалённая
  машина — Windows 11, юзер `beta\bberb`, консольная сессия активна.
- **Деплой-папка на beta**: `C:\Users\bberb\AppData\Local\Temp\lumepeer-dbg\`
  (`lumepeer-desktop.exe`, `lumepeer-decoder-worker.exe`, `tauri-pilot.exe`,
  `host.cmd`). Запуск через планировщик (задача `LumepeerDbg`) — чтобы процесс
  попал в интерактивную сессию с рабочим столом (sshd даёт сессию 0).
- **Полный цикл управления отработан**: `invite_create` на хосте →
  `invite_connect` у гостя → `session_grant` на хосте → гость в `connected`,
  открывается окно `view-*`. Всё через `tauri-pilot ipc` без ручного UI.

### Сборка
- Локальная сборка `lumepeer-desktop` со встроенным фронтендом:
  `npx tauri build --debug --no-bundle --features pilot,capture-windows,encode-openh264,encode-mf,decode-openh264`.
  Важно: обычный `cargo build` даёт бинарник, смотрящий на vite devUrl
  (`localhost:5173`) — это и были «таймауты eval» из questions.md п.1.
- Починен билд: `audiopus_sys` (vendored libopus) против CMake 4 — нужен
  `CMAKE_POLICY_VERSION_MINIMUM=3.5` (уже был в памяти, подтвердилось).
- Освобождено место на C: (диск был 100% полон — сборки падали):
  удалён `target/debug/incremental` (~3.8 ГБ).

### Найдена и исправлена реальная причина «картинки нет» №1: энкодер
- Симптом: `no encoder available: this session stays blank … no hardware
  encoder and the openh264 fallback is not built in`. На beta нет аппаратного
  MF-кодера, а openh264 не был включён в фичи. Исправлено пересборкой с
  `encode-openh264`.

### Найдена и исправлена реальная причина №2: захват экрана (код изменён!)
- Файл: `crates/media/src/capture/windows.rs`.
- Баг: DXGI Desktop Duplication не отдаёт ни одного кадра, пока рабочий стол
  не сделает первый present. На статичном экране (никто не трогает мышь)
  гость ждал бы картинку вечно. X11/macOS этим не страдают.
- Фикс: первый кадр теперь берётся GDI-снапшотом монитора
  (`gdi_snapshot()`: CreateDCW по `DeviceName` из DXGI_OUTPUT_DESC →
  CreateDIBSection → BitBlt, top-down BGRA8), поле `awaiting_first_frame`;
  дальше — обычный путь duplication.
- Тесты обновлены (`capture_produces_a_frame_when_a_display_is_available` —
  первый кадр обязан прийти сразу): 12/12 зелёные,
  clippy `-D warnings` чисто, `cargo fmt` прогнан. **Изменения в рабочем дереве,
  НЕ закоммичены.**

### Диагностика сети
- Оба endpoint'а видят релеи n0; путь нестабилен у обеих сторон
  (`Ping timeout` к euc1/use1, pkarr publish errors). Включён режим прямых
  путей: `LUMEPEER_LAN_DIRECT=1` на обеих сторонах (иначе relay-only).
- Контрольный хендшейк хоста ограничен 10 с (`CONTROL_HANDSHAKE_TIMEOUT_SECS`);
  при флапающем релее первые попытки не успевали — лечится повтором попыток
  (тикет жив ~10 мин, инвайты переиспользуемые).
- Шторм медиа-диалов гостя (~1/сек) выжигал 8 слотов хендшейков хоста
  (`handshake slots exhausted`) и мешал контрольному каналу. Причина шторма —
  зацикленный redial в `spawn_media_receiver` при пустом потоке.

---

## ✅ Цель достигнута (2026-08-24, продолжение)

**Картинка в окне гостя работает.** Живой сквозной прогон через интернет:
хост `beta` (schtasks, консольная сессия) → гость (эта машина), роль
`view_only`, одно медиа-соединение. Скриншоты окна `view-70c85fc264f7177a`,
снятые с интервалом, показывают удалённый рабочий стол и тикающие часы в
трее (15:28 → 15:29) — поток живой, а не один застрявший кадр.

### Причина №3 — та, из-за которой кадры не доходили (код изменён!)

- Файл: `apps/desktop/src-tauri/src/view.rs`.
- Баг: гость открывал на `rd/media/1` **два** соединения — одно из
  `stream_once` (видео), второе из `spawn_audio_receiver` (аудио). А хост
  пишет аудио тегированным стримом в **то же** соединение, что и видео:
  `on_audio_toggle` берёт `session.connection` из `self.media`.
- Что из этого выходило: для хоста второе соединение того же пира
  неотличимо от передиала, и `on_media_accepted` честно делает
  `previous.stop()` — то есть `abort()` энкод-лупа плюс `close()` прошлого
  соединения. Аудио-диал убивал видео-сессию через доли секунды после её
  старта. Гость видел «media stream ended», начинал новый проход — и
  спавнил **ещё один** вечный аудио-приёмник: они накапливались, каждый
  ретраился раз в секунду. Отсюда и 2075 «media connection accepted», и
  «handshake slots exhausted», и главное — полное отсутствие ошибок в логе
  хоста: луп не падал, его **отменяли**, а `abort()` не пишет ничего.
- Фикс: аудио больше не диалит своё соединение. `stream_once` сначала
  забирает видео-стрим, затем поднимает на **том же** соединении
  `spawn_audio_pass` (тегированные стримы через `accept_audio_media_stream`),
  а страж `AbortOnDrop` гасит его вместе с проходом — течь задач ушла.
- Регрессионный тест: `view::tests::one_media_pass_dials_one_connection` —
  поднимает два локальных endpoint'а и считает принятые медиа-соединения:
  ровно одно на проход.

### Проверено

- `cargo fmt --all -- --check` — чисто.
- `cargo clippy -p lumepeer-desktop --all-targets --features pilot,capture-windows,encode-openh264,encode-mf,decode-openh264 -- -D warnings` — чисто.
- `cargo clippy -p lumepeer-media --all-targets --features capture-windows,encode-openh264,decode-openh264 -- -D warnings` — чисто.
- `cargo test -p lumepeer-desktop` — 15/15 в `view::` (с новым тестом).
- `cargo test -p lumepeer-media --features capture-windows` — 49/49
  (включая GDI-фикс первого кадра из причины №2).
- Сквозной прогон: в логе хоста ровно **одна** строка «media connection
  accepted» (было 2075), ни одного WARN/ERROR по медиа за всю сессию;
  у гостя поднялся `lumepeer-decoder-worker.exe` и жёг CPU — кадры реально
  декодируются, а не просто приходят.

## 🔜 Что осталось (картинку не блокирует)

1. `awaiting_first_frame` живёт в капчурере, а не в медиа-сессии: взводится
   в `start()` (первый зритель), поэтому при **передиале посреди сессии**
   новый энкод-луп снова ждёт первого present'а вместо GDI-снимка. На живом
   рабочем столе спасают часы и курсор, но по-хорошему нужен способ
   перевзвести «первый кадр» на новую медиа-сессию.
2. `std::mem::forget(pcm_rx)` в `spawn_media_receiver` — заглушка: аудио
   декодируется, но устройства вывода у гостя всё ещё нет.
3. Один `sendmsg error: Os { code: 10040 }` (WSAEMSGSIZE) на Tailscale-пути
   в момент установления связи. На сессию не повлиял, но стоит запомнить.

## Побочные находки

- PowerShell-команды по ssh из bash: экранирование кавычек ломается —
  надёжнее звать `tauri-pilot.exe` напрямую или писать временные .cmd/.py.
- `$_.Name` в PowerShell через bash-ssh съедается bash'ем — использовать
  tasklist/findstr.
- **`tauri-pilot` не находит приложение, когда его запускает python**
  (`subprocess.run`, хоть со `shell=True`) — при полностью совпадающем
  окружении и том же .exe; из bash напрямую находит всегда. Именно поэтому
  `full-cycle.py` падал с «No active tauri-pilot instance found» на живом
  приложении. Локальные вызовы пилота делать из bash; ssh-часть — чем угодно.
- `cmake` в PATH нет; рабочий лежит в venv hermes-agent
  (`AppData/Local/hermes/hermes-agent/venv/Scripts/cmake.exe`, 4.4.2), и с
  ним обязателен `CMAKE_POLICY_VERSION_MINIMUM=3.5`.
- `cargo clippy -p lumepeer-desktop` падает в build.rs с «Access is denied»,
  пока запущен `lumepeer-decoder-worker.exe`: `tauri-build` стейджит сайдкар
  поверх работающего файла. Сначала гасить гостя, потом clippy.
- Скрипты-помощники в `%TEMP%`: `connect-guest.py`, `full-cycle.py`,
  `check-view.py`, `check-frames.py`, `analyze-log.py`, `invite-new.json`.
  Скриншоты живой сессии — в скретчпаде сессии (`view.png`, `view2.png`).
