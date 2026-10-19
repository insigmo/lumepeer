# Пачка 11 — PostgreSQL + pgvector: схема, индексы, индексатор, поиск, eval

**Репозиторий:** support_knowledge · **Зависит от:** 02, 03 (нужен реальный
корпус хотя бы одной пачки) · **Нужно:** Docker локально

## Цель

Воспроизводимый индекс корпуса в PostgreSQL: инкрементальная индексация по
хешам, контролируемая полная перестройка без простоя, гибридный поиск
(вектор + полнотекст + точные сигнатуры ошибок) с фильтрами применимости и
измеренное качество поиска.

## Прочитать перед началом

- `contracts.md` § 1 и § 4.2 (форма ответа поиска).
- Код пачки 02 (`src/kb/`, схема, словари) — индексатор переиспользует
  разбор frontmatter и Markdown из линтера, а не пишет второй.

## Стек

PostgreSQL 17 + pgvector ≥ 0.8 (нужно итеративное сканирование HNSW) +
`pg_trgm`. Образ `pgvector/pgvector` с **точным** тегом версии.
`psycopg` 3 + `pgvector` (Python-адаптер). Эмбеддинги —
`intfloat/multilingual-e5-large` локально на CPU: сначала проверить, есть ли
модель в `fastembed` (ONNX, лёгкий образ); если нет — `sentence-transformers`
с CPU-колёсами torch. Префиксы e5 обязательны: `passage: ` для фрагментов,
`query: ` для запросов. Миграции — простые SQL-файлы + маленький раннер
`kb db migrate` с таблицей версий; Alembic/ORM не нужны.

## Задачи

### 1. Локальная БД и роли

`deploy/docker-compose.dev.yml` с Postgres. Роли с минимальными правами:
`kb_indexer` (пишет `kb`, создаёт/меняет `kb_build`), `kb_api` (читает `kb`,
`vuln`; пишет только `kb_staging`), `kb_feeds` (пишет `vuln`). Схему
`gateway` создаёт и мигрирует сам шлюз (пачка 15) своей ролью.

### 2. Схема `kb` (создаётся индексатором из `sql/kb_schema.sql` с
параметром имени схемы — для `kb_build`)

Ориентир; имена и типы можно уточнить, **индексы и их назначение —
сохранить**:

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE {s}.index_meta (
  singleton       boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  embedding_model text NOT NULL,
  embedding_dim   integer NOT NULL,
  chunker_version integer NOT NULL,
  corpus_commit   text NOT NULL,
  built_at        timestamptz NOT NULL
);

CREATE TABLE {s}.documents (
  id               text PRIMARY KEY,
  path             text NOT NULL UNIQUE,
  title            text NOT NULL,
  document_type    text NOT NULL,
  os               text NOT NULL,
  families         text[] NOT NULL DEFAULT '{}',
  distributions    text[] NOT NULL DEFAULT '{}',
  versions         jsonb  NOT NULL DEFAULT '[]',
  categories       text[] NOT NULL,
  verification     text NOT NULL,
  lifecycle        text NOT NULL,
  risk_level       text NOT NULL,
  rollback         text NOT NULL,
  probe_ids        text[] NOT NULL DEFAULT '{}',
  action_ids       text[] NOT NULL DEFAULT '{}',
  check_ids        text[] NOT NULL DEFAULT '{}',
  sources          jsonb  NOT NULL,
  extends          text,
  frontmatter      jsonb  NOT NULL,        -- целиком, для ответа API
  body             text   NOT NULL,        -- Markdown без frontmatter
  content_hash     bytea  NOT NULL,
  last_verified_at date
);
-- фильтр применимости — самый частый предикат
CREATE INDEX ON {s}.documents (os, lifecycle, verification);
CREATE INDEX ON {s}.documents USING gin (families);
CREATE INDEX ON {s}.documents USING gin (distributions);
CREATE INDEX ON {s}.documents USING gin (categories);
-- точный поиск документа к проверке безопасности (/v1/checks/{id})
CREATE INDEX ON {s}.documents USING gin (check_ids);

CREATE TABLE {s}.chunks (
  id            text PRIMARY KEY,   -- {doc_id}#{section-slug}:{n} — стабилен при правке других разделов
  doc_id        text NOT NULL REFERENCES {s}.documents(id) ON DELETE CASCADE,
  ordinal       integer NOT NULL,
  heading_path  text NOT NULL,
  content       text NOT NULL,
  content_hash  bytea NOT NULL,
  -- денормализованные фильтры: ANN-сканирование фильтрует без JOIN
  os            text NOT NULL,
  lifecycle     text NOT NULL,
  families      text[] NOT NULL,
  distributions text[] NOT NULL,
  tsv tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', heading_path), 'A')
     || setweight(to_tsvector('english', content), 'B')
     || to_tsvector('simple', content)        -- коды, пути, имена команд без стемминга
  ) STORED,
  embedding vector(1024) NOT NULL
);
CREATE INDEX ON {s}.chunks (doc_id);
CREATE INDEX ON {s}.chunks (os, lifecycle);
CREATE INDEX ON {s}.chunks USING gin (tsv);
CREATE INDEX ON {s}.chunks USING hnsw (embedding vector_cosine_ops)
  WITH (m = 16, ef_construction = 64);

CREATE TABLE {s}.error_signatures (
  doc_id     text NOT NULL REFERENCES {s}.documents(id) ON DELETE CASCADE,
  signature  text NOT NULL,   -- как в документе
  normalized text NOT NULL,   -- нижний регистр, hex-коды к виду 0x%08x, пробелы схлопнуты
  PRIMARY KEY (doc_id, normalized)
);
CREATE INDEX ON {s}.error_signatures (normalized);                         -- точное совпадение
CREATE INDEX ON {s}.error_signatures USING gin (normalized gin_trgm_ops);  -- подстрока/опечатки
```

Почему так:

- HNSW, а не IVFFlat: не требует обучения на данных, корпус растёт
  инкрементально. Фильтры по ОС/семейству сужают выборку, поэтому в
  запросе — `SET LOCAL hnsw.iterative_scan = relaxed_order` и
  `hnsw.ef_search` (подобрать по eval), иначе фильтр после ANN теряет
  результаты.
- `vector(1024)` — размерность выбранной модели; при смене модели —
  полная перестройка (задача 4), а не `ALTER`.
- Полнотекст — `english` (корпус английский) + `simple` (коды ошибок,
  имена файлов). Русские запросы находит векторная часть и сигнатуры.
- Отдельная таблица сигнатур: код ошибки — самый точный сигнал, он не
  должен зависеть от стемминга и эмбеддингов.
- `versions` (jsonb) не индексируется: сравнение версий делается в коде
  после выборки кандидатов (форматы версий разные у разных `target`).

### 3. Разбиение на фрагменты

По разделам второго уровня. Фрагмент = `title` + путь заголовков + текст
раздела. Длинный раздел режется по абзацам до лимита модели (~400
токенов при окне 512); блок кода никогда не режется. `CHUNKER_VERSION` —
константа; её изменение требует полной перестройки.

### 4. Индексатор `kb index`

- **Инкрементально** (по умолчанию): хеш файла (с нормализованными
  окончаниями строк) против `documents.content_hash`; изменённый
  документ заменяется целиком в одной транзакции (документ + фрагменты +
  сигнатуры); удалённый — удаляется; неизменённый — не трогается и не
  эмбеддится заново. `index_meta.corpus_commit` пишется последним.
  Прерванный запуск безопасно перезапускается.
- Если `embedding_model`, `embedding_dim` или `chunker_version` в
  `index_meta` отличаются от текущих — инкрементальный режим отказывает
  и требует `--full`.
- **Полная перестройка** `kb index --full`: строит схему `kb_build`,
  проверяет (число документов = числу файлов корпуса, у каждого ≥ 1
  фрагмента, smoke-запросы из eval находят ожидаемое), затем в одной
  транзакции `kb` → `kb_old`, `kb_build` → `kb`, `DROP SCHEMA kb_old`.
  Живой индекс до свопа не трогается. Тест: API-запрос после свопа идёт в
  новую схему (подготовленные запросы не держат старую).
- Перед индексацией — `kb lint` (невалидный корпус не индексируется).
- `--dry-run` печатает, что изменится.

### 5. Поиск `kb search` (функция, которую в пачке 12 вызовет API)

1. Нормализовать `error_messages` → точное и триграммное совпадение в
   `error_signatures`.
2. Векторные кандидаты (top 40) и полнотекстовые (`websearch_to_tsquery`,
   top 40) с фильтрами: `lifecycle = 'active'`, `os IN (<os>, 'any',
   'lumepeer')`, семейство/дистрибутив — пустой массив или содержит
   целевое значение.
3. Слияние на уровне документа — Reciprocal Rank Fusion (k = 60) по трём
   спискам; бонусы за `verification` и `document_type` — константы в одном
   месте, значения подобраны по eval, не на глаз.
4. Применимость по `versions` в коде: `matches` / `unknown` / `excluded`
   (excluded не возвращается; unknown — возвращается с пометкой).
5. Статус ответа: `ok`, `no_results`, `low_relevance` (порог — по
   негативным запросам eval), `unsupported_platform`. Ошибка БД — исключение,
   а не пустой список.

### 6. Семантический сигнал в `kb dedup`

Добавить в существующую команду пачки 02 сигнал «близкие эмбеддинги
документов» (по `title` + `symptoms` + `Causes`) — тем же кодом
эмбеддингов. Отдельного инструмента нет.

### 7. Eval поиска

`eval/queries.yaml` — не меньше 60 запросов: русские и английские,
перефразировки, точные коды ошибок, запросы с «чужой» ОС (Linux-проблема
при `os: windows` — не должна возвращать Linux-документы), запросы без
ответа в корпусе (ожидается `no_results`/`low_relevance`). Ожидаемые id —
из реального корпуса; формулировки — как пишут пользователи (форумы), а
не копии заголовков.

`kb eval` считает recall@3, recall@5, MRR, долю правильных отказов на
негативных и пишет `eval/results/<дата>.md` (модель, коммит корпуса,
параметры). Порогов «должно быть ≥ X» не выдумывать — фиксируется базовая
линия, дальше сравнение с ней.

### 8. Тесты и CI

pytest с настоящим Postgres: в CI — сервис-контейнер GitHub Actions; локально
— compose. Если `KB_TEST_DSN` задан, а БД недоступна, тест **падает**, а не
пропускается. Тесты: инкрементальность (изменение одного документа меняет
только его строки; повторный запуск без изменений — ноль записей и ноль
эмбеддингов), удаление документа, отказ инкрементального режима при смене
модели, полная перестройка и своп, фильтры ОС/семейства, точное совпадение
кода ошибки, `low_relevance`.

## Чего НЕ делать

- Не добавлять второй поисковый движок, внешний векторный сервис,
  LangChain/LlamaIndex, ORM.
- Не использовать внешний API эмбеддингов.
- Не индексировать один и тот же документ дважды (variant-документ —
  отдельный документ, это нормально; копия — нет).
- Не делать HTTP-эндпоинт переиндексации — только CLI.
- Не хранить индекс в Git; не коммитить модель.

## Definition of done

- [ ] `kb index --full` из чистого клона собирает индекс; повторный `kb
      index` — ноль изменений (вывод в отчёте).
- [ ] Изменение одного документа переиндексирует только его (тест +
      ручная проверка с выводом).
- [ ] `EXPLAIN (ANALYZE, BUFFERS)` каждого запроса поиска (вектор, FTS,
      сигнатуры) на реальном корпусе — в отчёте; видно, какие индексы
      используются.
- [ ] `kb eval` выполнен, файл результатов закоммичен, в отчёте — цифры и
      худшие промахи с разбором.
- [ ] Тесты зелёные с настоящей БД.
