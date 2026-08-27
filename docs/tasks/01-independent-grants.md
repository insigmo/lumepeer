# 01 — Независимые гранты: сделать их выдаваемыми

**Зависимости:** нет. **Блокирует:** `02`, `03`, `06`, `10`.

## Почему это первое

`Grants` (`crates/core/src/consent.rs`, ~стр. 41) объявляет шесть независимых
разрешений. `Grants::from_role` жёстко ставит `clipboard_read`,
`clipboard_write`, `file_transfer` и `recording` в `false`, а **метода, который
мог бы их изменить, в `SessionManager` не существует** (`crates/core/src/session.rs`:
есть `grant`, `revoke`, `grants`, `active` — мутатора нет).

Следствие: код в `apps/desktop/src-tauri/src/network.rs`, который проверяет
`g.clipboard_read` (~стр. 1765, 2366), `v.grants.clipboard_write` (~стр. 2363) и
`g.recording` (~стр. 2005), **всегда получает `false`**. Буфер обмена, передача
файлов и запись сессии недостижимы не из-за отсутствия UI, а из-за отсутствия
пути авторизации в TCB.

## Ключевое проектное решение

Новый мутатор работает **только с четырьмя независимыми грантами**. `view` и
`input` остаются производными от `Role` и меняются только через
`grant`/`revoke`. Так UI физически не может выдать `input`, минуя смену роли.

## Задача 1 — `IndependentGrant` и мутатор в TCB

**Файлы:** `crates/core/src/consent.rs`, `crates/core/src/session.rs`

Делать:
- В `consent.rs` добавить `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]`
  `pub enum IndependentGrant { ClipboardRead, ClipboardWrite, FileTransfer, Recording }`
  с `#[serde(rename_all = "snake_case")]`, как у соседнего `ControlAction`.
- Дать `Grants` метод `pub const fn get(self, which: IndependentGrant) -> bool`
  и `pub fn set(&mut self, which: IndependentGrant, allowed: bool)`. Оба — по
  одному `match` на все четыре варианта, без `_ =>`: когда кто-то добавит пятый
  грант, компилятор обязан это заметить.
- В `session.rs` добавить
  `pub fn set_grant(&mut self, peer: NodeId, which: IndependentGrant, allowed: bool) -> Result<()>`.
  Правила: сессия должна быть `Active` (иначе `CoreError::NotPermitted`, а не
  паника и не молчаливый `Ok`); изменение применяется к снапшоту грантов этой
  сессии; `revoke` обязан сбрасывать всё обратно в `Grants::default()`.

Не делать:
- Не добавлять `view`/`input` в `IndependentGrant`.
- Не менять `Grants::from_role`. Её контракт («роль не подразумевает ничего
  из четырёх») остаётся ровно как есть.
- Не трогать сериализацию `Role` и `MessageKind` — это ломает golden vectors.

Готово, когда: юнит-тесты в `session.rs` закрывают все четыре гранта, отказ на
`Pending`/`Ended`/неизвестном пире, и то, что `revoke` обнуляет гранты.

## Задача 2 — Аудит изменения гранта

**Файл:** `crates/core/src/audit.rs`

Добавить вариант `AuditEvent::GrantChanged { grant: IndependentGrant, enabled: bool }`
(новый вариант в конец enum). Записывать его из `set_grant`, если у
`SessionManager` уже есть доступ к sink; если нет — не изобретать проводку,
просто добавить вариант и вернуть событие вызывающему, а реальный sink
подключается в `16-audit-log.md`.

Не логировать содержимое буфера, имена файлов и сырой `NodeId` — §15.

## Задача 3 — Проводка actor → IPC

**Файлы:** `apps/desktop/src-tauri/src/network.rs`,
`apps/desktop/src-tauri/src/commands.rs`, `apps/desktop/src-tauri/src/main.rs`,
`apps/desktop/src-tauri/capabilities/main.json`

Образец для копирования — цепочка `monitor_select`: вариант в `enum ActorCommand`
(~стр. 206) → метод на `ActorHandle` (~стр. 658) → приватный `on_*` на `Actor`
(~стр. 2188) → `#[tauri::command]` в `commands.rs` → строка в
`invoke_handler!` в `main.rs` → `"allow-session-set-grant"` в `main.json`.

- Команда называется `session_set_grant`, аргументы — структура
  `SessionSetGrantArgs { peer: String, grant: IndependentGrant, allowed: bool }`.
- Разрешена **только из главного окна**: `check_window(&window)?`, без
  `check_view_window`. Гость не выдаёт себе гранты.
- `SessionStatusDto` (`commands.rs`, ~стр. 264) должен отдавать текущее
  состояние четырёх грантов, иначе UI нечего показывать.

Не делать: не добавлять команду в `capabilities/view.json`.

## Задача 4 — UI на стороне хоста

**Файлы:** `apps/desktop/src/session-status.ts`, `apps/desktop/src/i18n.ts`,
тесты рядом

- Для каждой активной сессии — четыре переключателя, подписанные так, чтобы
  человек понимал последствие: «Гость может читать мой буфер обмена», а не
  «clipboard_read».
- Все выключены, пока хост не включил. После `session_revoke` — сброс.
- Ключи i18n добавить и в `en`, и в `ar` (`i18n.test.ts` падает иначе).
- Переключатели должны быть достижимы с клавиатуры и иметь `aria-label` —
  `accessibility.test.ts` и `keyboard-nav.test.ts` это проверяют.

## Задача 5 — Тесты

- `tests/integration/tests/consent_cycle.rs`: сессия получает `file_transfer`,
  затем `revoke` — грант исчез; повторный `grant` не восстанавливает его.
- Property-тест в `crates/core` (там уже используется `proptest` по машине
  состояний §8.1): никакая последовательность `set_grant` не может включить
  `view` или `input`.

## Проверка

```sh
cd apps/desktop && npm install && npm run build && npm run typecheck && npm test && cd - && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

## Ловушки

- `Grants` помечен `#[allow(clippy::struct_excessive_bools)]` с обоснованием
  — не «чинить» это, превращая поля в битфлаги.
- `MessageKind` трогать не нужно: гранты не ездят по проводу, каждая сторона
  проверяет свою копию. Если покажется, что нужно новое сообщение — сначала
  прочитай `crates/core/src/protocol.rs` про `PROTOCOL_MINOR` и `FEATURE_*`.
- ADR обязателен: это расширение модели авторизации §8.2. Свободный номер
  бери после чтения `15-docs-and-adr-debt.md`.
