# Пачка 02 — схема знаний, словари, линтер, дубликаты, шаблоны, CI

**Репозиторий:** support_knowledge · **Зависит от:** 01 · **Нужно:** —

## Цель

Сделать так, чтобы плохой документ нельзя было влить: схема метаданных,
словари допустимых значений, линтер структуры, поиск кандидатов в дубликаты
и CI. Плюс три образцовых документа, по которым пишутся пачки 03–10.

## Прочитать перед началом

- `contracts.md` целиком (поля § 1.2, id § 1.1, каталог § 2, проверки § 3).
- Результаты пачки 01: `research/source-selection.md`, `sources/catalog.yaml`,
  `coverage/plan.md`.

## Стек

Python 3.12, `uv` (lock-файл в Git), один пакет `kb` с одной CLI `kb`
(подкоманды добавляют следующие пачки: `index`, `search`, `serve`,
`proposals`, `feeds`). Зависимости этой пачки: `pyyaml`, `jsonschema`,
`markdown-it-py`, `pytest`, `ruff`. Больше ничего без причины.

## Задачи

### 1. Каркас

`pyproject.toml`, `src/kb/`, `tests/`, `ruff` в режиме проверки и формата,
`kb --help`. Каталоги `knowledge/<os>/<area>/` создавать только вместе с
первым документом в них.

### 2. Схемы

- `schemas/knowledge-entry.schema.json` (JSON Schema 2020-12) — поля из
  `contracts.md` § 1.2, плюс:
  - `required_privileges`: `user` | `admin` (что нужно для шагов
    исправления);
  - для `document_type: security_check` — обязательные `check_ids`,
    `check_type` (`passive` | `local` | `network_self`), `reads` (категории
    читаемых данных из словаря), `false_positive_risk` (`low`|`medium`|`high`),
    `needs_separate_consent` (bool);
  - для `failed_attempt` — обязательные разделы (см. задачу 4).
  Поля, которых нет в контракте, добавлять только с записью в
  `contracts.md` (в lumepeer) в том же PR.
- `schemas/security-finding.schema.json` — по `contracts.md` § 3.1.
- `schemas/source-catalog.schema.json` — для `sources/catalog.yaml`.
- `versions` — машинно сравнимая форма: `{target, min?, max?}`, где
  `target` — `windows-10`, `windows-11`, `macos`, `ubuntu`, `debian`, … из
  словаря; формат версий для каждого `target` описан в словаре
  (`11-24H2`, `14.5`, `24.04`, `13`).

### 3. Словари `vocab/*.yaml`

`os.yaml`, `families.yaml`, `distributions.yaml` (с семейством и форматом
версии), `areas.yaml`, `probe-ids.yaml`, `action-ids.yaml`, `check-ids.yaml`
(копии из `contracts.md` § 2–3 с одной строкой описания), `data-categories.yaml`
(для `reads`). Линтер принимает только значения из словарей.

### 4. Линтер `kb lint`

Проверяет каждый `knowledge/**/*.md`:

- frontmatter валиден по схеме; значения — из словарей;
- `id` уникален; верхний каталог совпадает с `os`; `extends` /
  `superseded_by` ссылаются на существующие id;
- `probe_ids`, `action_ids`, `check_ids` есть в словарях;
- обязательные разделы по типу (заголовки второго уровня, точные
  английские имена):
  - `playbook`: `Symptoms`, `Applies to`, `Causes`, `Diagnosis`, `Fix`,
    `Verify`, `Risks`, `Rollback`, `Sources`; раздел `Causes` различает
    подтверждённые причины и гипотезы;
  - `security_check`: `What is checked`, `Why it matters`, `How it is read`,
    `Interpreting results`, `Remediation`, `Rollback`, `Limitations`,
    `Sources`;
  - `failed_attempt`: `What was tried`, `Conditions`, `Expected`,
    `Actual`, `Known cause`, `Sources`;
  - `reference`, `anti_pattern`, `incident_case` — свой минимальный набор,
    определить и записать в `templates/`;
- каждый блок кода имеет язык (`powershell`, `cmd`, `bash`, `zsh`, `text`);
- размер тела ≤ константы `MAX_DOC_TOKENS` (оценка по словам; значение
  обосновать: документ целиком уходит модели — пачка 11);
- `verified` / `partially_verified` требуют непустых `sources`, и домен
  каждого URL источника есть в `sources/catalog.yaml` (значит, условия
  использования проверены);
- поиск секретов и персональных данных: приватные ключи, токены
  распространённых форматов, `password=`, e-mail, домашние пути с именем
  пользователя, IP вне документационных диапазонов (RFC 5737 / 3849 —
  разрешены);
- цитаты длиннее порога (блоки `>` и кавычки) — предупреждение «проверь,
  не копия ли это».

Стабильность id в CI: `kb lint --base origin/master` — id, который был в
базе и исчез из head, — ошибка (документ не удаляют, а помечают
`deprecated`).

`kb links --online` — отдельная команда (не в CI): проверяет, что внешние
ссылки открываются, и пишет отчёт.

### 5. Дубликаты `kb dedup`

Одна команда, несколько сигналов (пачка 11 добавит семантический сигнал в
эту же команду, второго инструмента не будет):

- одинаковые нормализованные `error_signatures` у документов с
  пересекающимися ОС/версиями;
- пересечение множеств команд из блоков кода (Jaccard) выше порога;
- близость заголовков и `symptoms` (триграммы).

Вывод — пары-кандидаты с причиной. В CI (`kb dedup --ci`) — ошибка, если
пара выше строгого порога и не записана в `dedup/allow.yaml` с
объяснением разницы условий. Похожесть — не повод сливать: решение
«объединить / дополнить / оставить отдельно» принимает человек или пачка
контента, и оно записывается.

### 6. Шаблоны и образцы

`templates/<document_type>.md` — пустые шаблоны с комментариями, что писать
в каждом разделе (правило «команда → нужные права → ожидаемый вывод →
последствия»).

Три образцовых документа, написанные полностью по правилам, с реальными
источниками:

- `windows.network.dns-resolution` (playbook, ссылается на
  `net.interfaces`, `net.connectivity`, `net.flush_dns`,
  `net.set_dns_servers`);
- `linux.packages.dpkg-interrupted` (playbook, семейство `debian`);
- `windows.security.firewall-profiles` (security_check,
  `check_ids: [win.firewall.profiles]`).

Пачки 03, 05, 06 эти темы не повторяют.

### 7. CI

`.github/workflows/ci.yml`: `uv sync --frozen`, `ruff check`, `ruff format
--check`, `kb lint --base`, `kb dedup --ci`, `pytest`. Без секретов.
Тесты линтера и дубликатов — на фикстурах в `tests/fixtures/` (плохие и
хорошие документы), не на реальном корпусе.

### 8. `CONTRIBUTING.md`

Как писать документ: выбрать тип, найти существующий (`kb dedup --against
<черновик>` — сначала поиск), правило «одно решение — одно место», общие
решения на уровне ОС/семейства, отличия дистрибутива — отдельным
`variant`-документом с `extends`; правило «свои слова, не копия»; уровни
подтверждения — дословно по `docs/ai-support/content-workflow.md` в
lumepeer (пересказать, не придумывать свои).

## Чего НЕ делать

- Не писать индексатор, эмбеддинги, БД, API — это 11–12.
- Не делать второй инструмент дубликатов или линтер на другом языке.
- Не добавлять поля в схему «на будущее» (коммерческие флаги, рейтинги,
  счётчики просмотров).
- Не наполнять корпус сверх трёх образцов.
- Не тащить pre-commit-фреймворки, Makefile-обёртки, Docker для линтера.

## Definition of done

- [ ] `uv run kb lint` зелёный на трёх образцах; на фикстурах каждая
      проверка задачи 4 имеет красный и зелёный тест.
- [ ] `uv run kb dedup` находит подложенный дубликат в фикстурах и не
      находит у трёх образцов.
- [ ] `kb lint --base` ловит исчезнувший id (тест).
- [ ] CI-файл проходит локально тем же набором команд.
- [ ] Изменения в `contracts.md` (если были) сделаны в lumepeer в том же
      цикле и перечислены в отчёте.
