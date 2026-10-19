# Общие контракты AI Support

Здесь только то, что связывает несколько пачек. Пачка, которая вводит новый
элемент (новую пробу, действие, проверку, поле), добавляет его сюда в том же
PR. Пачка, которая только использует элемент, его не переименовывает.
Удаление и переименование запрещены: устаревшее помечается, новое получает
новый id (как протокол Lumepeer — растёт только в хвост).

---

## 1. Документы базы знаний

### 1.1. Идентификатор

`<os>.<area>.<slug>[.<variant>]`, только `[a-z0-9-]` и точки.

- `os`: `windows`, `macos`, `linux`, `any` (кросс-платформенное),
  `lumepeer` (про само приложение).
- `area`: из словаря `vocab/areas.yaml` (пачка 02). Начальный набор: `boot`,
  `updates`, `network`, `bluetooth`, `audio`, `display`, `peripherals`,
  `storage`, `performance`, `packages`, `apps`, `services`, `accounts`,
  `crashes`, `security`, `remote-access`, `backup`.
- `variant` — только для дополнения к общему документу с реально другими
  командами: `linux.network.dns-resolution.fedora` расширяет
  `linux.network.dns-resolution` (поле `extends`).

id не меняется после слияния в `master`. Документ, который заменили,
получает `lifecycle: deprecated` и `superseded_by: <новый id>`.

### 1.2. Поля frontmatter, на которые опираются индексатор, API и клиент

Окончательную JSON-схему пишет пачка 02 (`schemas/knowledge-entry.schema.json`);
эти поля и их смысл она сохраняет.

| Поле | Тип | Смысл |
|---|---|---|
| `id` | string | см. 1.1 |
| `title` | string | английский заголовок |
| `document_type` | enum | `reference`, `playbook`, `verified_solution`, `failed_attempt`, `anti_pattern`, `incident_case`, `security_check` |
| `os` | enum | как в id |
| `families` | string[] | `debian`, `fedora`, `arch`, `suse` (для Linux), иначе `[]` |
| `distributions` | string[] | из `vocab/distributions.yaml`; `[]` = все в семействе |
| `versions` | object[] | машинно сравнимые ограничения: `{target, min?, max?}`; без прозы |
| `categories` | string[] | из `vocab/areas.yaml` |
| `verification` | enum | `verified`, `partially_verified`, `unverified` |
| `lifecycle` | enum | `active`, `deprecated` |
| `risk_level` | enum | `low`, `medium`, `high` — риск применения инструкции |
| `rollback` | enum | `full`, `partial`, `none`, `not_applicable` |
| `required_privileges` | enum | `user`, `admin` — что нужно для шагов исправления |
| `symptoms` | string[] | короткие английские фразы |
| `error_signatures` | string[] | дословные коды/тексты ошибок (`0x800f081f`, `NXDOMAIN`) |
| `probe_ids`, `action_ids` | string[] | ссылки на каталог из раздела 2 |
| `check_ids` | string[] | ссылки на проверки из раздела 3 |
| `sources` | object[] | `{url, type, retrieved_at}`; `type` ∈ `official`, `vendor-repo`, `community`, `article`, `advisory` |
| `language` | enum | `en` |
| `extends`, `superseded_by` | string? | id другого документа |
| `created_at`, `updated_at`, `last_verified_at` | date | |

У `security_check` дополнительно: `check_type` (`passive`, `local`,
`network_self`), `reads` (категории данных из словаря), `false_positive_risk`,
`needs_separate_consent`.

`verified` значит: утверждения подтверждены в указанных `versions` и
условиях, а не «работает везде». `failed_attempt` — только тип документа,
никогда не уровень подтверждения.

---

## 2. Каталог инструментов устройства v1

Каталог живёт в `crates/core/src/support.rs` (пачка 14) и является
единственным источником: из него шлюз строит описания инструментов для
модели, исполнитель — проверку параметров, база знаний — словарь
`vocab/probe-ids.yaml` / `vocab/action-ids.yaml` (копируется руками при
изменении; линтер проверяет ссылки).

### 2.1. Классы риска и отката

- **Класс риска** (`risk`): `read` — ничего не меняет; `change` — меняет
  настройку или состояние, предсказуемо; `dangerous` — удаление данных,
  права доступа, установка/удаление ПО, защитные механизмы, загрузка ОС,
  системные файлы.
- **Откат** (`undo`): `not_needed` — не оставляет состояния (очистка кэша
  DNS); `exact` — состояние «до» сохранено и восстанавливается точно;
  `partial` — восстанавливается не всё (что именно — в описании);
  `checkpoint` — откат только через контрольную точку уровня 2;
  `irreversible`.
- **Политика** (решает `crates/core`, не модель и не UI):
  - `read` — выполняется без подтверждения, если выдано разрешение на
    диагностику;
  - `change` + `not_needed`/`exact` — подтверждение карточкой «что
    изменится, как отменить»;
  - `change` + `partial`/`irreversible` — подтверждение с явным текстом
    ограничений;
  - `dangerous` — только при подготовленной и проверенной контрольной точке
    или точной резервной копии; без неё — отказ с объяснением и
    предложением безопасной альтернативы; отдельное осознанное
    подтверждение ограниченного действия допускается только там, где пачка
    это явно разрешает.
- Флаги: `needs_elevation`, `disrupts_session` (рвёт удалённую сессию —
  в удалённом режиме запрещено, пока пачка 20 не определит путь),
  `needs_network`, `slow` (может идти минуты).

### 2.2. Пробы (risk = `read`)

| id | Что возвращает | ОС |
|---|---|---|
| `sys.info` | ОС, версия, сборка, архитектура, аптайм, RAM, CPU, версия Lumepeer. Без имени компьютера и пользователя | все |
| `sys.disk_usage` | тома, ФС, размер, свободно | все |
| `sys.top_processes` | top-N по CPU/RAM: только имена и цифры, без командных строк | все |
| `sys.startup_items` | автозапуск (Run/Startup/включённые службы; LaunchAgents/Login Items; autostart/systemd units) | все |
| `sys.services` | состояние указанной службы или список упавших | все |
| `sys.recent_errors` | ошибки системного журнала за окно, сгруппированные по источнику/id, с лимитом | все |
| `sys.crash_reports` | недавние падения приложений (имя, модуль, код), без дампов | все |
| `net.interfaces` | адаптеры, состояние, локальные адреса, шлюз, DNS-серверы. Публичный IP и MAC — нет | все |
| `net.connectivity` | шлюз, разрешение имени системным резолвером, HTTPS до известного адреса — коды результата | все |
| `net.wifi` | состояние, уровень сигнала, драйвер. SSID — нет | все |
| `net.listening_ports` | слушающие порты + имя процесса | все |
| `hw.problem_devices` | устройства с ошибками драйвера (коды PnP; модули без драйвера) | Windows, Linux |
| `hw.audio_devices` | устройства вывода/ввода, устройство по умолчанию, состояние аудиослужбы | все |
| `hw.display` | мониторы, GPU, версия драйвера | все |
| `pkg.pending_updates` | ожидающие обновления ОС, отдельно — обновления безопасности, возраст метаданных | все |
| `pkg.find` | установлена ли программа/пакет по имени и её версия. Полного списка ПО модель не получает | все |
| `pkg.manager_health` | целостность пакетного менеджера / хранилища компонентов (без исправления) | все |
| `fs.read_config` | содержимое файла из **закрытого списка** известных конфигов (hosts, resolv.conf, sources.list.d, …), с маскированием | все |
| `lumepeer.log_tail` | хвост собственного лога Lumepeer, с маскированием | все |
| `security.basic_check` | запуск базовой проверки безопасности, возвращает находки — **вводит пачка 19** | все |

### 2.3. Действия

| id | Что делает | risk | undo | ОС |
|---|---|---|---|---|
| `net.flush_dns` | сброс кэша DNS | change | not_needed | все |
| `net.renew_dhcp` | обновить аренду DHCP на адаптере | change | not_needed; disrupts_session | все |
| `net.restart_adapter` | выключить/включить адаптер | change | not_needed; disrupts_session | все |
| `net.set_dns_servers` | задать DNS адаптеру или вернуть «автоматически» | change | exact | все |
| `net.hosts_remove_entry` | убрать строку из hosts | change | exact | все |
| `net.winsock_reset` | сброс Winsock (нужна перезагрузка) — **вводит пачка 18** | dangerous | checkpoint | Windows |
| `svc.restart` | перезапустить службу | change | not_needed | все |
| `svc.start` / `svc.stop` | запустить / остановить службу | change | exact | все |
| `svc.set_startup` | тип запуска службы | change | exact | все |
| `startup.disable` / `startup.enable` | отключить/включить элемент автозапуска тем же механизмом, что и системный UI | change | exact | все |
| `proc.terminate` | завершить процесс | change | irreversible | все |
| `disk.clean_temp` | перенести временные файлы в зону удержания Lumepeer (очистка — отдельно, по сроку) | change | exact | все |
| `print.clear_queue` | перезапуск спулера, файлы очереди — в зону удержания | change | exact | Windows, Linux (CUPS) |
| `audio.restart_service` | перезапуск аудиослужбы (Audiosrv / PipeWire user units / coreaudiod) | change | not_needed | все |
| `audio.set_default_device` | устройство по умолчанию | change | exact | все |
| `time.resync` | синхронизация времени (и включение NTP с сохранением прежнего значения) | change | exact | все |
| `pkg.repair` | DISM RestoreHealth + SFC / `dpkg --configure -a` + `apt-get -f install` / аналоги — **вводит пачка 18** | dangerous | checkpoint | все |
| `pkg.install_os_updates` | установить обновления ОС — **вводит пачка 18** | dangerous | checkpoint | все |
| `av.quick_scan` | быстрая проверка штатным антивирусом | change | not_needed | Windows |
| `av.update_signatures` | обновить сигнатуры штатного антивируса | change | not_needed | Windows |

Пачки 18 и 19 добавляют: `checkpoint.create`, `checkpoint.restore`,
`quarantine.isolate`, `quarantine.restore`, `av.scan_file`,
`sec.firewall_enable`, `sec.smb1_disable`, `sec.guest_disable` — с классами,
которые они определят.

### 2.4. Правила реализации, общие для всех проб и действий

- Процессы запускаются **только** списком аргументов (`Command::new` +
  `.arg`), без `sh -c`/`cmd /c`. PowerShell-скрипты — константы в коде;
  параметры от модели передаются через переменные окружения и проверены
  строгим enum/regex **до** запуска. Строка от модели никогда не
  склеивается с текстом скрипта.
- У каждого запуска есть тайм-аут, лимит вывода и путь остановки.
- Вывод проходит маскирование до сохранения, показа и отправки: токены,
  ключи, пароли, PSK, e-mail, домашний путь (`C:\Users\<имя>` → `~`),
  имя компьютера, серийные номера, публичные IP, MAC.
- Результат действия — факт, подтверждённый проверкой состояния после
  выполнения, а не код выхода команды.

---

## 3. Проверки безопасности v1

Каждой проверке соответствует документ `document_type: security_check` с
`check_ids: [<id>]` в базе знаний (пачки 05, 08, 09) и реализация в движке
(пачка 19).

**Windows:** `win.update.cu_age`, `win.update.pending_security`,
`win.os.support_status`, `win.av.realtime`, `win.av.signatures_age`,
`win.firewall.profiles`, `win.disk.bitlocker_os`, `win.boot.secure_boot`,
`win.uac.level`, `win.smb1.enabled`, `win.rdp.exposure`,
`win.accounts.guest_enabled`, `win.accounts.autologon`,
`win.net.listening_ports`, `win.apps.outdated`.

**macOS:** `mac.update.os_version`, `mac.update.auto_security`,
`mac.os.support_status`, `mac.sip`, `mac.gatekeeper`, `mac.disk.filevault`,
`mac.firewall`, `mac.sharing.remote_login`, `mac.sharing.screen_sharing`,
`mac.accounts.autologin`, `mac.net.listening_ports`.

**Linux:** `linux.update.pending_security`, `linux.update.metadata_age`,
`linux.update.auto`, `linux.os.support_status`, `linux.firewall.active`,
`linux.ssh.root_login`, `linux.ssh.password_auth`, `linux.disk.encryption`,
`linux.boot.secure_boot`, `linux.net.listening_ports`.

**Общее:** `any.kev.matches` — CVE из неустановленных обновлений, которые
есть в CISA KEV.

### 3.1. Находка (finding)

Схема — `schemas/security-finding.schema.json` в support_knowledge (пачка 02)
и зеркальный тип в `crates/core` (пачка 19). Три независимых оценки
никогда не сливаются в одно поле:

- `status`: `confirmed`, `potential`, `not_affected`, `not_checked`,
  `unknown`;
- `severity` (техническая серьёзность): `info`, `low`, `medium`, `high`,
  `critical`;
- `confidence` (уверенность, что это про это устройство): `low`, `medium`,
  `high`;
- отдельно: `remediation_available: bool`, `evidence` (короткие факты),
  `limitations`, `sources` (URL первоисточника), `data_as_of` (дата
  актуальности данных), `checked_at`, `needs_elevation`, `reason` для
  `not_checked`/`unknown`.

Недостаточно данных → `not_checked` или `unknown`, **никогда** «уязвимостей
нет». Совпадение части номера версии — не `confirmed`.

---

## 4. API

### 4.1. ai-gateway (публичный, пачка 15) — единственная точка входа клиента

| Метод | Путь | Назначение |
|---|---|---|
| GET | `/health` | живость; без версий зависимостей и внутренних адресов |
| POST | `/v1/ai/turn` | один ход модели, SSE-поток **нейтральных** событий (см. ниже) |
| POST | `/v1/kb/search` | прокси в kb-api `/v1/search` |
| GET | `/v1/kb/documents/{id}` | прокси в kb-api |
| GET | `/v1/security/baseline` | прокси в kb-api (пачка 13) |
| GET | `/v1/security/kev` | прокси в kb-api (пачка 13) |
| POST | `/v1/feedback` | прокси в kb-api (пачка 22) |

Аутентификация: заголовки `Lumepeer-Device` (публичный ключ узла, hex),
`Lumepeer-Timestamp` (unix-секунды), `Lumepeer-Signature` — Ed25519 над
`"lumepeer-ai-gateway-v1\n" || method || "\n" || path || "\n" || timestamp || "\n" || blake3(body)`.
Окно времени и защита от повтора — пачка 15. На сервере хранится только
`blake3(публичный ключ)`, не сам ключ.

#### Нейтральный протокол хода (клиент ↔ шлюз)

Клиент не знает, какой провайдер отвечает. Окончательные JSON-схемы пишет
пачка 15 в `services/ai-gateway/docs/api.md`; эта форма сохраняется.

Запрос: `conversation` — `{route?, catalog_fingerprint, capabilities[],
mode, locale, os_summary}` (`route` пуст у нового разговора) и `messages[]`:

- `{role: "user", content: [{type: "text", text}]}`;
- `{role: "assistant", content: [{type: "text", text} | {type: "tool_call",
  id, tool, input}], provider_payload}` — `provider_payload` непрозрачен
  для клиента (у Anthropic там исходные блоки ответа, включая `thinking` с
  подписью) и возвращается шлюзу **без изменений**;
- `{role: "user", content: [{type: "tool_result", call_id, is_error,
  output}]}` — все результаты одного хода в одном сообщении.

История только дописывается.

События SSE: `route {route}` (первое событие нового разговора; клиент
сохраняет и присылает дальше), `text_delta {text}`, `tool_call {id, tool,
input}` (целиком, после разбора JSON), `message_end {stop, refusal_category?,
provider_payload, usage}` где `stop` ∈ `end_turn`, `tool_calls`,
`max_tokens`, `refusal`, `error {code, retryable}`.

Разговор закреплён за маршрутом. Если маршрут выведен из работы, шлюз может
продолжить разговор на другом маршруте, отбросив `provider_payload`
(нейтральная часть истории самодостаточна) — кэш и сохранённое мышление при
этом теряются, корректность — нет.

Имена инструментов у провайдеров ограничены `[a-zA-Z0-9_-]`, поэтому шлюз
отображает id каталога в имя функции заменой `.` на `__`
(`net.flush_dns` ↔ `net__flush_dns`) и обратно; клиент видит только id
каталога.

### 4.2. kb-api (внутренний, пачки 12–13) — доступен только шлюзу

| Метод | Путь | Назначение |
|---|---|---|
| GET | `/health` | БД доступна, индекс собран, `corpus_commit`, `embedding_model` |
| POST | `/v1/search` | гибридный поиск, см. ниже |
| GET | `/v1/documents/{id}` | документ целиком с метаданными |
| GET | `/v1/checks/{check_id}` | документ к проверке безопасности |
| GET | `/v1/security/baseline` | для `os` + `release`/`version` + `arch` + `build`: последняя сборка/версия ветки, отставание, KEV-пересечение, статус поддержки, `data_as_of`, `stale` |
| GET | `/v1/security/kev` | компактный список id из CISA KEV с `ETag` — для локального сопоставления |
| POST | `/v1/feedback` | предложение в `kb_staging`, никогда — в корпус |

Сервисный токен со скоупами (`read`, `feedback`). Переиндексации, изменения
корпуса и повышения статуса через HTTP **нет** — только CLI на сервере.

`/v1/search` запрос: `query` (обязательно), `system {os, family?,
distribution?, version?, architecture?}`, `symptoms[]`, `error_messages[]`,
`attempted_doc_ids[]`, `limit`. Ответ: `results[]` с `id`, `title`,
`document_type`, `verification`, `lifecycle`, `applicability` (почему
подходит / что не установлено), `matched_sections[]`, `risk_level`,
`rollback`, `action_ids`, `sources[]`, `score` (как реально посчитан), и
`status` ответа: `ok`, `no_results`, `low_relevance`, `unsupported_platform`.
Ошибка сервиса — HTTP-ошибка, не пустой список.

---

## 5. Что может покинуть устройство

| Куда | Что | Когда |
|---|---|---|
| шлюз → провайдер модели маршрута (Anthropic или другой — экран согласия называет фактического) | сообщения пользователя, результаты проб/действий после маскирования, краткая сводка ОС, список доступных инструментов | при каждом ходе AI Support, после согласия |
| шлюз → kb-api | текст запроса поиска, ОС/семейство/дистрибутив/версия | когда модель вызывает поиск |
| шлюз → kb-api | `os` + `release`/`version` + `arch` + номер сборки | запрос baseline для проверки безопасности |
| шлюз → kb-api | обезличенный итог ремонта, показанный пользователю целиком | только по явному согласию на каждую отправку (пачка 22) |

Никогда: резервные копии, содержимое пользовательских файлов, полный
инвентарь ПО, отчёт безопасности целиком, имя пользователя и компьютера,
SSID, MAC, серийные номера, публичный IP, содержимое буфера обмена,
скриншоты (пока не появится отдельное согласие на скриншот).
