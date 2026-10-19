# Пачка 13 — данные об уязвимостях: KEV, MSRC, macOS, сроки поддержки; baseline

**Репозиторий:** support_knowledge · **Зависит от:** 12 (01 — выбор и
лицензии источников) · **Нужно:** веб, Docker

## Цель

Дать устройству эталон, с которым оно сравнивает себя **локально**: какая
последняя сборка/версия ОС для его ветки, сколько он отстаёт, какие из
пропущенных исправлений закрывают уязвимости из CISA KEV, поддерживается
ли его версия ОС. Данные внешних источников — машиночитаемые таблицы в
схеме `vuln`, не Markdown и не Git.

## Прочитать перед началом

- `research/security-assessment.md`, `sources/catalog.yaml` (блок
  `security_feeds`) — пачка 01 уже проверила адреса и условия. Источник,
  условия которого не подтверждены, не подключать — спросить владельца.
- `contracts.md` § 3, § 4.2, § 5.

## Источники v1 (только эти)

| Источник | Что даёт | Таблицы |
|---|---|---|
| CISA KEV (JSON) | CVE, которые реально эксплуатируются | `vuln.kev` |
| MSRC Security Update Guide API (CVRF v3) | по месяцам: продукты Windows → KB, `FixedBuild`, CVE, флаг эксплуатации | `vuln.windows_builds`, `vuln.windows_month_cves` |
| Данные о выпусках macOS — SOFA (macadmins) при подтверждённой лицензии, иначе страница Apple security releases | последняя версия каждой ветки, CVE выпуска, активно эксплуатируемые | `vuln.macos_releases`, `vuln.macos_release_cves` |
| endoflife.date API | даты поддержки Windows, macOS, дистрибутивов | `vuln.os_lifecycle` |

Linux-пакеты в v1 сервер не сопоставляет: это делает родной пакетный
менеджер на устройстве (пачки 08 и 19).

## Задачи

### 1. Схема `vuln` (миграция)

- `feeds(source PK, last_attempt_at, last_success_at, etag, last_modified,
  records, last_error)` — свежесть каждого источника.
- `kev(cve_id PK, vendor_project, product, name, date_added, due_date,
  known_ransomware boolean)`.
- `windows_builds(release, arch, build, ubr, kb, released_on, month_id,
  PRIMARY KEY (release, arch, ubr))` + индекс `(release, arch, released_on DESC)`
  — «последняя сборка ветки» и «сборки после моей» одним проходом индекса.
- `windows_month_cves(month_id, release, arch, cve_id, max_severity,
  exploited boolean)` + индексы `(release, arch, month_id)` и `(cve_id)`.
- `macos_releases(major, version, released_on, is_security)` +
  `macos_release_cves(version, cve_id, actively_exploited)`; индексы по
  `(major, released_on DESC)` и `(cve_id)`.
- `os_lifecycle(product, cycle, released_on, support_until, eol_on, lts,
  latest, PRIMARY KEY (product, cycle))`.

Хранится минимум из `research/security-assessment.md`: идентификатор,
оригинальный URL, даты, версии/сборки, флаги. Описания CVE целиком, CVSS-
векторы, сырые дампы — не хранить.

### 2. Синхронизация `kb feeds sync [--source]`

Отдельный загрузчик на источник, общий каркас: условные запросы
(`ETag`/`If-Modified-Since`), честный `User-Agent` с контактом, лимиты и
повторы по правилам источника, запись в одной транзакции на источник,
обновление `feeds`. Сбой источника не стирает прежние данные и не делает их
«свежими». Запуск — CLI по расписанию сервера (cron/systemd timer → `docker
compose run --rm`), описать в `docs/operations.md`. Константа свежести на
источник; превышение → источник `stale`.

Сопоставление Windows: релиз в данных MSRC называется названием продукта
(«Windows 11 Version 24H2 for x64-based Systems») — нормализовать в
`release` + `arch` из словаря; ненормализуемые продукты — в лог, не
угадывать. Внеочередные (out-of-band) выпуски учитывать.

### 3. Эндпоинты baseline

- `GET /v1/security/baseline?os=windows&release=24H2&arch=x64&build=26100.4061`
  → последняя сборка и KB ветки, дата, на сколько выпусков/месяцев
  отстаёт указанная сборка, число CVE в пропущенных выпусках, список CVE
  из KEV среди них (только id), статус поддержки ветки, `data_as_of`,
  `stale`.
- `GET /v1/security/baseline?os=macos&version=14.6.1` — аналогично по
  ветке.
- `GET /v1/security/baseline?os=linux&distribution=ubuntu&release=24.04` —
  только статус поддержки.
- `GET /v1/security/kev` — компактный список id KEV (+ дата, флаг
  ransomware) с `ETag`, чтобы устройство сопоставляло локально CVE из
  своих пакетных метаданных и ничего о них не сообщало серверу.

Форма запросов уже записана в `contracts.md` § 4.2 и § 5 (baseline
получает `build`/`version` — минимальные неидентифицирующие данные); если
реализация требует отличий — исправить контракт в том же цикле.

Правила ответа: неизвестный релиз → `unsupported_release`, а не «всё
хорошо»; `stale` источник → поле `stale: true`, клиент обязан показать
ограничение; сборка новее известной → `ahead_of_data` (данные отстают),
не «уязвимостей нет».

### 4. Документы корпуса

`knowledge/any/security/how-update-status-is-determined.md` (reference):
откуда эталон, как часто обновляется, что значит `stale`/`unknown`, почему
«последняя сборка» не равно «нет уязвимостей». На него ссылаются документы
проверок `*.update.*`.

### 5. Тесты

На маленьких сохранённых образцах каждого источника (`tests/fixtures/feeds/`;
проверить, что условия источника позволяют хранить образец — иначе
синтетический образец той же структуры): разбор, нормализация продуктов
MSRC, расчёт отставания, `ahead_of_data`, `unsupported_release`, `stale`,
частичное совпадение версии не даёт «исправлено», сбой загрузки не
обновляет `last_success_at`.

## Чего НЕ делать

- Не подключать NVD, OSV, трекеры дистрибутивов, EPSS — не в v1.
- Не сопоставлять сторонние приложения по CPE.
- Не принимать от устройства список установленного ПО или пакетов.
- Не превращать описания уязвимостей в «шаги эксплуатации» или
  исполняемые действия.
- Не коммитить дампы источников.

## Definition of done

- [ ] `kb feeds sync` наполняет все четыре источника из сети (в отчёте —
      число записей и `data_as_of` каждого).
- [ ] Baseline для реальной сборки этой машины (Windows 11) отвечает
      правдоподобно; в отчёте — сверка с официальной страницей Windows
      release health вручную.
- [ ] Тесты зелёные; `contracts.md` соответствует реализации.
