# Пачка 09 — Переключение: `asInvoker`, автозапуск, передача после входа

**Репозиторий:** lumepeer · **Зависит от:** 04, 05, 06, 07, 08 (все слиты) ·
**Нужно:** этот ПК, `beta`, стандартная учётная запись Windows

Прочитать до начала: ADR 0169 §2, §9, «What does not change»; ADR 0057;
ADR 0140; ADR 0164. Код: `apps/desktop/src-tauri/build.rs` (`APP_MANIFEST`),
`apps/desktop/src-tauri/src/autostart.rs`, `apps/desktop/src-tauri/src/main.rs`
(`claim_host_role`, запуск рантайма хоста), `crates/service/src/client_launch.rs`,
`crates/service/src/session_host_launch.rs` (пачка 02),
`crates/terminal-worker/src/main.rs` (ссылки на `requireAdministrator`).

## Задачи

### 1. Манифест

`requestedExecutionLevel level="asInvoker"`. Doc-комментарий `APP_MANIFEST`
переписать: почему теперь `asInvoker` и где теперь права (ADR 0169).
Зависимость Common-Controls v6 — оставить.

### 2. Клиент не хостит в релизе на Windows

- Релизная сборка Windows не поднимает хост в процессе. Нет службы или
  session-host не запущен → в окне «этот компьютер сейчас недоступен для
  подключений» и кнопка «Запустить службу» (UAC, `sc start`).
- Debug-сборка и `LUMEPEER_KEYSTORE=file` — хостинг в процессе как раньше
  (`e2e/matrix`). Проверить, что матрица не сломалась.

### 3. Маркер снят

Session-host больше не требует `session-host-enabled`; код маркера и
`SESSION_HOST_EXIT_NOT_ENABLED` — удалить (если не используются иначе).

### 4. Автозапуск (ADR 0140 → ADR 0169 §9)

- Клиент (трей) — снова ключ `Run` текущего пользователя.
- Задача планировщика ADR 0140 удаляется при первом запуске новой версии;
  если удалить без прав нельзя — оставить отказ в логе и снять её в
  `--access-settings` или установщиком (решить по факту и записать).
- «Принимать подключения после входа» (хранилище session-host, ADR 0169 §1):
  по умолчанию — как сейчас автозапуск при первом запуске; значение ставит
  сам session-host, когда создаёт личность. **Включить** позже — расширение
  (машина доступна без открытого окна), только через `--access-settings`;
  **выключить** — сужение, через канал пачки 03. Переключатель «Запускать с
  Windows» в клиенте управляет только трей-автозапуском.

### 5. Передача после входа (ADR 0164)

`client_launch.rs` — запуск клиента **ограниченным** токеном и только как
интерфейс; хостинг после входа — session-host. Комментарии о
`requireAdministrator` в `client_launch.rs`, `host_role.rs`, `autostart.rs`,
`crates/terminal/src/windows.rs`, `crates/terminal-worker/src/main.rs` —
переписать под новое положение.

### 6. Документы

- `docs/comparison-rustdesk-teamviewer-anydesk.md` — строка «Запуск без прав
  администратора».
- `docs/gap-backlog.md` № 17 — закрыт ADR 0169.
- `docs/release-checklist.md` — шаги живой проверки из ADR 0169
  «Verification».
- ADR 0057 — в шапке «§1 superseded by ADR 0169»; ADR 0140, 0164, 0114,
  0123, 0085 — строка «Amended by ADR 0169».

## Definition of done

- [ ] Запуск клиента на этом ПК — без UAC; Process Explorer / `whoami
      /groups` из дочернего процесса — Medium.
- [ ] Стандартная учётка: клиент стартует без пароля администратора, машина
      доступна гостю.
- [ ] Перетаскивание файла из Проводника в окно клиента работает.
- [ ] Перезагрузка → экран входа (логон-хост) → вход → гость перезванивает
      в session-host той же личности.
- [ ] `e2e/matrix` на Windows-хосте зелёный в той же мере, что до пачки.
- [ ] Повышенный `cargo test`, `npm test`, `fmt`, `clippy`.

## Статус (2026-10-29)

Код — ветка `unelev/09-switch-to-asinvoker` (worktree `../lumepeer-09`,
поверх `6db86a9`), **не закоммичено**. Неповышенно: `cargo test --workspace`
1089 ok / 1 красный — `a_captured_frame_reaches_the_guest_through_the_sandboxed_decoder`,
он красный и в базовом прогоне на чистом дереве (там 1091 ok; разница −2 —
удалённые тесты маркера и выбора владельца); `npm test` 993 (в базе 990),
`typecheck`, `ipc-contract`, `fmt --check`, `clippy -D warnings` по пяти
затронутым крейтам и по десктопу с `capture-windows,encode-mf`. Повышенный
прогон и все живые пункты DoD **не сделаны**: нужен UAC, установленная
NSIS-сборка, `beta` и стандартная учётка.

Где что:

- манифест — `build.rs`: `asInvoker`, Common-Controls v6 оставлен;
- «клиент не хостит» — `placement::session_host_guest()`: релизная сборка
  без `LUMEPEER_KEYSTORE=file`; debug и file-keystore хостят в процессе;
- маркер — удалён из `program_data.rs`, `session_host.rs`,
  `session_host_launch.rs`, `logon_host_launch.rs`, `access_settings.rs`;
  код выхода 6 (`SESSION_HOST_EXIT_NOT_ENABLED`) удалён и **не переиспользован**;
- «недоступен» — `HostPanel::unreachable()` (нет ответа session-host
  `SESSION_HOST_UNREACHABLE_SECS` = 5 с), поле `host_unreachable` в
  `network_status`, баннер и кнопка «Запустить службу» в `main.ts`, команда
  `service_start` → `lumepeer-service.exe --install` повышенно (один UAC);
- `service_control::ensure_installed` больше не повышается сам: делает это
  только уже повышенный процесс (иначе это UAC при каждом старте на машине
  без службы);
- автозапуск — `autostart.rs`: снова ключ `Run` текущего пользователя;
- «Принимать подключения после входа» — запрос канала
  `AcceptAfterSignInOff` (0x17, сужение), поле `accept_after_sign_in` в
  снимке хоста и в `unattended_status`, команда
  `unattended_set_accept_after_sign_in` (вкл. — только окно
  `--access-settings`, выкл. — канал), переключатель в окне доступа и
  строка состояния с кнопкой «выкл.» в клиенте;
- `client_launch.rs` — только `start_client` ограниченным токеном;
  `start_owner_client`, `elevated_primary` и запасной путь на
  `ERROR_ELEVATION_REQUIRED` удалены.

Что сделано не буквально по тексту пачки:

- **Задачу планировщика ADR 0140 снимает session-host, а не клиент.**
  Проверено на этом ПК: ACL задачи `\Lumepeer\Autostart-beta_win` даёт
  пользователю только `FR`, неповышенный клиент удалить её не может. Если
  снимать её установщиком, выбор «автозапуск включён» теряется: установщик
  срабатывает раньше первого запуска нового клиента. Session-host — `SYSTEM`
  в сессии человека, его куст `HKU\<SID>` загружен. При старте он пишет туда
  значение `Run` (если его нет) и только после этого удаляет задачу
  (`autostart::retire_logon_task`).
- **Личность создаёт session-host при первом подключении клиента.**
  Раньше её создавал только повышенный клиент, и на чистой установке после
  `asInvoker` session-host навсегда уходил бы в `NO_STORE`. Теперь при
  пустом хранилище он остаётся закрытым, а при первом подключении клиента
  открывает endpoint (`load_or_create` создаёт ключ) и ставит
  `accept-after-sign-in` — по умолчанию «вкл.», как автозапуск
  (ADR 0103). `NO_STORE` остаётся только для хранилища, которое нельзя
  открыть или прочитать. У тех, кто обновляется, личность уже есть, поэтому
  `accept-after-sign-in` у них «выкл.»: машина доступна, пока трей-клиент
  из `Run` подключён, — как и до пачки.
- **`OP_INJECT_DESKTOP` остаётся** — см. пачку 10.
