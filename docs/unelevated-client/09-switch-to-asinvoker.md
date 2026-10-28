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
