# 01 — Rust-тесты в CI и релиз только при зелёном CI

**Блокер аудита P1.** **Зависит от:** ничего. **Параллельно:** с любой пачкой
(интерфейс не трогает).

## Что есть (проверено 2026-11-08 на `b97a6c9`)

- `.github/workflows/ci.yml` — jobs `fmt`, `ui`, `media` (Linux, только
  `lumepeer-media`, `lumepeer-runtime` по фильтру, два интеграционных теста и
  `lumepeer-net` с secret-service), `fuzz`, `supply-chain`. **Нет ни
  `cargo test --workspace`, ни `cargo clippy --workspace`, ни Windows и macOS.**
- Их убрал коммит `eb90617` «fix issues» (2026-09-17, −144 строки, только
  `ci.yml`): job `build` с матрицей `ubuntu-latest`/`windows-latest`/
  `macos-latest` (зависимости Linux, `npm run build`, сайдкары
  `lumepeer-decoder-worker` и `lumepeer-service` в `binaries/`, `fmt`,
  `clippy --workspace --all-targets -D warnings`, `build --workspace`,
  `test --workspace`) и job сквозной сессии (две сборки приложения на одном
  Xvfb, настоящий iroh и QUIC, запись экрана хоста; удалённый комментарий
  ссылается на ADR 0036, файла которого в `docs/adr/` нет). Посмотреть целиком: `git show eb90617 -- .github/workflows/ci.yml`.
  Тем же днём закрыт «unix connect cluster» — вероятно, job’ы убрали, чтобы
  CI позеленел; причину стоит выяснить, а не повторить.
- `.github/workflows/release.yml` запускается на **каждый push в `master`**
  (`on.push.branches: ["master"]`), цепочка `guard → version → build →
  manifest …`, и **ни одна** из них не ждёт CI. Доказательство: на `79887e6`
  и на `b97a6c9` CI красный, release зелёный и опубликован.
- `guard` пропускает только коммит `chore: release v…`. Коммит, меняющий одни
  документы, выпускает новую версию без изменений в коде, и она уходит
  пользователям автообновлением.
- CI сейчас красный из-за одного шага: `cargo deny check` →
  `error[yanked]: async-executor 1.14.0` (`Cargo.lock`, запись около строки 268).
- Локально (Windows, без повышения прав) `cargo test --workspace`: 1090 из
  1092, два падения похожи на окружение:
  - `tests/integration/tests/media_pipeline.rs` ищет воркер как файл рядом с
    тестом (`path.push("lumepeer-decoder-worker")`, около строки 33) и **не
    пересобирает** его. Запустился `lumepeer-decoder-worker.exe` от 14 октября,
    собранный ещё с openh264 (до ADR 0162).
  - `crates/service/src/host_channel.rs` около строки 418, тест канала
    LocalSystem (`sid == "S-1-5-18"`). На машине работает установленный
    `lumepeer-service` от SYSTEM с тем же каналом. Вероятно, тест сталкивается
    с ним.
- `crates/aom-sys/build.rs` около строки 188 намеренно паникует без `nasm`
  (ADR 0141). На раннерах `nasm` должен быть установлен явно.

## Задача 1 — yanked-крейт

1. Поднять `async-executor` до неотозванной версии. **Не** `cargo update` целиком
   и осторожно с `cargo update -p`: в этом репозитории он пересобирает
   несвязанные рёбра. Если `-p async-executor --precise <версия>` тянет
   лишнее, править `Cargo.lock` руками (версия + checksum) и проверить
   `cargo tree -i async-executor`.
2. `cargo deny check` локально зелёный (предупреждения `duplicate` и
   `unmatched-skip` не ошибки, их не трогать в этой пачке).

## Задача 2 — вернуть матрицу

1. Восстановить job `build (${{ matrix.os }})` из `eb90617` с поправками на
   то, что изменилось с тех пор:
   - фичи `encode-openh264` больше нет (ADR 0162), вместо неё `encode-aom`, как
     в нынешнем job’е `media`;
   - `nasm` на всех трёх ОС (`apt install nasm`, `choco install nasm` или
     `winget`, `brew install nasm`), иначе `aom-sys` паникует;
   - на Windows libopus без `ninja` собирается с `/Od` (ADR 0149). Для тестов
     это не важно, но `ninja` дешевле поставить, чем потом объяснять скорость;
   - Linux-образ брать тот же, что у `media` и `release.yml` (после ADR про
     Ubuntu 26.04: pipewire 0.10 / libclang 22).
2. `cargo clippy --workspace --all-targets -- -D warnings` и
   `cargo test --workspace` на трёх ОС. Если что-то красное — чинить; если
   тест принципиально не может идти на раннере, то
   `#[ignore = "<почему>"]` с причиной, а не удаление job’а.
3. Job сквозной сессии из `eb90617` вернуть, если он укладывается в разумное
   время; если нет — записать в `docs/release-checklist.md`, что его нет и
   почему.

## Задача 3 — тесты, зависящие от окружения

1. `media_pipeline.rs`: устаревший воркер не должен давать загадочную ошибку
   декодирования. Минимум — тест проверяет, что найденный воркер собран
   из того же дерева (например, сравнить версию/метку сборки или время
   изменения с бинарником теста), и падает с сообщением
   «rebuild it: cargo build -p lumepeer-decoder-worker». Лучше, если тест
   сам гарантирует свежий воркер; выбрать то, что проще и надёжнее.
2. `host_channel.rs`: тест не должен зависеть от того, установлен ли на машине
   сервис. Например, уникальное имя канала для теста или явный пропуск с
   сообщением, когда канал LocalSystem уже занят. Сначала **доказать**
   причину (запуск с остановленным сервисом), потом чинить.
   Учесть: локально тесты идут без повышения прав, в CI — с правами
   администратора, DACL-тесты там ведут себя иначе.

## Задача 4 — релиз ждёт CI

1. Сделать так, чтобы `release.yml` не публиковал ничего, пока CI на том же
   коммите не зелёный. Предпочтительно: `ci.yml` получает `on: workflow_call`,
   а `release.yml` вызывает его job’ом `ci` (`uses: ./.github/workflows/ci.yml`)
   и `version` получает `needs: [guard, ci]`. Альтернатива — `workflow_run` с
   проверкой `conclusion == 'success'`; она сложнее с тегами и ручным
   запуском. Ручной `workflow_dispatch` тоже должен проходить через CI.
2. Не выпускать версию из коммита, который трогает только документы:
   `paths-ignore` (`docs/**`, `**/*.md`) на `push` в `release.yml` — или
   проверка в `guard`. Тег `v*` и ручной запуск должны работать как раньше.
3. Коммит поднятия версии по-прежнему от `beta <qwantone@gmail.com>`
   (`release.yml`, около строки 133) — не менять.
4. Обновить раздел «Release pipeline» в `docs/release-checklist.md`.

## Приёмка

- На push в `master` CI запускает `cargo test --workspace` на Windows, Linux и
  macOS, и все три зелёные.
- Нарочно красный тест в ветке (проверить и откатить) → release не доходит
  до `version`.
- `cargo deny check` зелёный.
- Коммит только в `docs/` не выпускает новую версию.
- `media_pipeline` со старым воркером в `target/` падает с понятным
  сообщением (или сам пересобирает его); `host_channel` проходит на машине с
  установленным сервисом.

## Проверки

```bash
cargo deny check
CARGO_INCREMENTAL=0 __COMPAT_LAYER=RunAsInvoker NASM="C:/Users/beta_win/AppData/Local/bin/NASM/nasm.exe" cargo test --workspace
```

Проверить запуск CI в GitHub (`gh run list --branch master`, `gh run view <id>`)
после push. Если для этого нужен push в `master`, задачи 1–3 вливать до
задачи 4 — иначе не на чем проверить, что релиз ждёт.
