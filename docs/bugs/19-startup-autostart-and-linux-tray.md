# 19 — Запуск: автозапуск по умолчанию и падение трея на Linux

**Пункты пользователя:** 2 и приложенный лог.
**Зависит от:** ничего. **Нужно:** Linux-ВМ (`debian`) для пункта 2; Windows
для пункта 1.

**Файлы:**
- `apps/desktop/src-tauri/src/autostart.rs`
- `apps/desktop/src-tauri/src/main.rs`
- `apps/desktop/src-tauri/tauri.conf.json`
- `apps/desktop/src-tauri/installer-hooks.nsh`
- `apps/desktop/src/system-settings.ts`

Две задачи в одной пачке, потому что обе — про то, что происходит, когда
приложение стартует, и обе трогают `main.rs` и упаковку. По коду они не
пересекаются и делать их можно в любом порядке.

---

## 1. Автозапуск должен быть включён по умолчанию

### Симптом со слов пользователя

«сделай так чтобы по умолчанию чекбокс о запуске приложения после активации
ОС включенным. нужно чтобы при перезагрузке включался и люмпир. Это можно
выключить в настройках, но должно быть включенным по умолчанию»

### Что происходит сейчас (проверено по коду)

Механизм есть и работает — ADR 0042, `autostart.rs`: `HKCU\…\Run` на Windows,
`~/Library/LaunchAgents/…plist` на macOS, `~/.config/autostart/….desktop` на
Linux. Панель настроек читает реальное состояние каждый раз
(`Autostart::is_enabled`), и это правильно.

Включает его не приложение, а установщик. И вот здесь дыра:

| Платформа | Кто включает | Работает? |
|---|---|---|
| Linux (deb/rpm) | `packaging/deb-postinst.sh`, `rpm-post.sh` → `su -l "$target_user" -c 'lumepeer-desktop --enable-autostart'` | да |
| macOS | `Autostart::reconcile_first_launch`, вызывается из `main.rs` | да |
| **Windows** | **никто** | **нет** |

`installer-hooks.nsh` устанавливает только службу (`--install`) и не зовёт
`--enable-autostart`. При этом doc-комментарий `reconcile_first_launch`
утверждает обратное — «Windows and Linux both get autostart from a hook that
runs exactly once (the NSIS installer/uninstaller …)». Для Windows это
неправда, такого хука нет.

Дописать вызов в NSIS-хук **нельзя**: `installMode: perMachine`, установщик
идёт с повышением, и `HKCU` в нём — это куст администратора, а не того, кто
потом сядет за машину. Ровно от этой ловушки deb-postinst защищается через
`su -l "$target_user"`.

### Что сделать

**Задача 1.1 — ADR 0102** (или следующий свободный, если `18` уже занял
0102). Решение одной строкой: автозапуск включается **один раз, при первом
запуске установленной копии, на всех платформах**, а не установщиком. Что в
нём должно быть:

- почему не установщик на Windows (куст не того пользователя);
- почему это не отменяет ADR 0042: переключатель остаётся, выключение
  по-прежнему удаляет запись целиком, и **повторно приложение её не
  включает** — за это отвечает маркер;
- что именно означает «первый запуск»: маркер есть → не трогать ничего.

**Задача 1.2 — обобщить `reconcile_first_launch` на все платформы.**
Сегодня и сама функция, и `first_launch_marker`, и вызов в `main.rs` — под
`#[cfg(target_os = "macos")]`.

- `first_launch_marker()` получает путь на каждой платформе. Брать его
  оттуда же, откуда приложение берёт остальные свои файлы, — рядом с
  конфигом (см. `crates/runtime/src/config.rs` и `disk.rs`), а не в
  `~/Library/LaunchAgents`. На macOS путь **не менять**: у установленных
  копий там уже лежит маркер, и смена пути включит автозапуск повторно тем,
  кто его осознанно выключил.
- Общая часть — «маркера нет и автозапуск выключен → включить, затем
  записать маркер» — становится платформонезависимой.
- Уборка мёртвого login item (`stale`) остаётся macOS-only: на Windows и
  Linux за это отвечают деинсталляторы.
- В `main.rs` убрать `#[cfg(target_os = "macos")]` с вызова и поправить
  комментарий над ним — он сейчас описывает ровно то, чего больше не будет.

**Задача 1.3 — умолчание в UI.** `apps/desktop/src/system-settings.ts`,
`autostart: false` в начальном состоянии. Это не баг (реальное состояние
приезжает из `autostartStatus()` сразу за первым рендером), но пользователь
видит момент, когда галка снята. Проверить, что после 1.2 первый кадр панели
на свежей установке уже показывает её включённой; если мигает — отложить
рендер до ответа `autostartStatus`, а не заводить второе умолчание.

**Задача 1.4 — тесты.** В `autostart.rs` тест на то, что при существующем
маркере `reconcile_first_launch` ничего не включает. Существующий тест
`autostart_can_be_turned_off_on_this_machine` не трогать.

### Чего НЕ делать

- Не писать в `HKLM` и не заводить системную службу ради автозапуска —
  `autostart.rs` объясняет, почему это другая фича с другими ставками.
- Не включать автозапуск при каждом старте. Маркер — это и есть разница
  между «по умолчанию включено» и «нельзя выключить».
- Не трогать `--enable-autostart` / `--disable-autostart` и хуки deb/rpm: они
  работают, и через них же чинится машина, где маркер уже стоит.

---

## 2. Приложение не стартует на Linux: нет ayatana-appindicator

### Симптом

Приложенный лог, `beta@debian`:

```
thread 'main' (3707) panicked at libappindicator-sys-0.9.0/src/lib.rs:41:5:
Failed to load ayatana-appindicator3 or appindicator3 dynamic library
libayatana-appindicator3.so.1: cannot open shared object file: No such file or directory
...
   6: libappindicator_sys::app_indicator_new
   7: <tray_icon::platform_impl::platform::TrayIcon>::new
   8: <tray_icon::TrayIconBuilder>::build
  13: <lumepeer_desktop::main::{closure#2} …>
Aborted (core dumped)
```

Приложение не запускается вообще. Не деградирует, не работает без трея —
`Aborted`.

### Причина (установлена по коду, гипотез нет)

Два независимых дефекта, каждый из которых достаточен:

**2a. Зависимость не объявлена.** `tauri.conf.json`:

```json
"linux": { "deb": { "depends": ["libpipewire-0.3-0"], … } }
```

`libayatana-appindicator3-1` там нет, в `rpm.depends` — тоже. Поэтому
`install.sh` (`dpkg -i` + `apt-get install -f -y`) ничего не подтягивает:
незаявленную зависимость чинить нечем.

**2b. Отсутствие трея валит приложение.** `main.rs::install_tray` возвращает
`tauri::Result`, и код рассчитан на деградацию — есть даже ветка «no bundled
window icon: the tray entry will be blank». Но `libappindicator-sys`
**паникует** внутри `TrayIconBuilder::build`, а паника — не `Err`. Комментарий
в `install_tray` при этом говорит, что трей — единственный путь назад к
интерфейсу, потому что закрытие окна его прячет. То есть на Linux без
индикатора приложение либо падает (сейчас), либо стало бы неубиваемо
спрятанным (если просто проглотить ошибку).

### Что сделать

**Задача 2.1 — объявить зависимость.** В `tauri.conf.json`:

- `deb.depends` += `libayatana-appindicator3-1`;
- `rpm.depends` += `libayatana-appindicator3` (проверить точное имя пакета в
  той RPM-дистрибуции, на которую мы ориентируемся, и записать в `docs/
  platform-support.md`, если оно расходится).

Это чинит новые установки и ничего не чинит на машине пользователя, где
пакет уже стоит — поэтому 2.2 обязательна.

**Задача 2.2 — падение трея не должно валить приложение.** Обернуть вызов
`TrayIconBuilder::build` в `std::panic::catch_unwind`, и при панике —
`tracing::error!` с внятным текстом («трея не будет; поставьте
libayatana-appindicator3-1») и продолжение запуска.

Вместе с этим — **окно не должно прятаться в несуществующий трей**. Обработчик
закрытия в `main` прячет окно вместо уничтожения; если трея нет, закрытие
окна обязано завершать приложение, иначе пользователь получит процесс, до
которого нельзя добраться. Флаг «трей есть» кладётся в состояние приложения
рядом с остальными и читается обработчиком закрытия.

**Задача 2.3 — проверить на живой ВМ.** `debian` (см.
`project_lumepeer_linux_build_envs`): сначала `apt-get remove` индикатора и
запуск — приложение должно **подняться** с предупреждением в логе и
закрываться крестиком; затем установка пакета и запуск — трей на месте.

### Чего НЕ делать

- Не заменять `tray-icon`/`libappindicator` на другую библиотеку.
- Не чинить `libEGL warning: egl: failed to create dri2 screen` из того же
  лога — это VMware без 3D-ускорения, к падению отношения не имеет.
- Не убирать трей на Linux совсем.

---

## Definition of done

- [x] ADR на «автозапуск включается при первом запуске» написан, номер взят
      грепом по ссылкам из кода. — **ADR 0103**, `docs/adr/0103-autostart-is-
      turned-on-once-by-the-app-itself.md` (0102 уже занят пачкой 18).
- [x] `reconcile_first_launch` работает на Windows, Linux и macOS; путь
      маркера на macOS не изменился.
- [x] Свежая установка на Windows: после первой перезагрузки приложение
      поднимается само; галка в настройках стоит. — проверено до перезагрузки
      включительно: первый запуск пишет `HKCU\…\Run\Lumepeer`; сам цикл
      «установщик + reboot» не прогонялся, см. «Чего не проверяли».
- [x] Выключенная вручную галка не включается обратно при следующем запуске.
      — проверено на живых Windows и Linux, обе платформы.
- [x] `deb.depends` и `rpm.depends` называют пакет индикатора.
- [x] На Linux без `libayatana-appindicator3-1` приложение **запускается**,
      пишет предупреждение и закрывается крестиком; с пакетом — трей
      работает как раньше. Проверено на ВМ, в отчёте — обе команды и обе
      реакции.
- [x] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
      -- -D warnings`, `cargo test --workspace`. — clippy и тесты зелёные;
      `fmt --check` красный на **чужом** файле, см. ниже.
- [x] `cd apps/desktop && npm run typecheck && npm test`. — 775 тестов, 29
      файлов, зелено.

---

## Что было сделано

**1. Автозапуск.** `reconcile_first_launch` перестал быть macOS-only:
маркер и «нет маркера и выключено → включить» переехали в общий код
(`reconcile_with_marker`), macOS-only осталась только уборка мёртвого login
item. Путь маркера на macOS не изменился
(`~/Library/LaunchAgents/io.insigmo.lumepeer.first-launch`), на Windows и
Linux это `autostart-first-launch` в `config_dir()`. Комментарий в `main.rs`,
который утверждал про несуществующий NSIS-хук, переписан. UI: `autostart`
стал `boolean | null`, и строка с галкой не рисуется, пока `autostartStatus`
не ответил — второго умолчания не заводили.

**2. Трей.** `catch_unwind` из задачи 2.2 **не годится**: в
`[profile.release]` стоит `panic = "abort"` (корневой `Cargo.toml`), и паника
внутри `libappindicator-sys` убивает процесс до раскрутки стека — ловить
нечего. Вместо этого `install_tray` сам пробует загрузить библиотеку **до**
`TrayIconBuilder::build`: те же четыре имени, в том же порядке, теми же
флагами `RTLD_LOCAL | RTLD_LAZY` (`dlopen2::raw::Library::open` — безопасная
функция, а крейт под `#![forbid(unsafe_code)]`; `dlopen2` уже тянет `tao` в
каждую Linux-сборку). Не загрузилось — `tracing::error!` с названиями пакетов
и `Ok(false)`; флаг `tray` лёг в `AppState`, и обработчик закрытия при нём
пропускает закрытие вместо `hide()`.

## Проверено на живой машине

Linux — WSL Debian 13 (trixie) с WSLg, живая сессия X11/Wayland.

```
# 1. пакет на месте: приложение поднимается, индикатор создаётся
$ ls /usr/lib/x86_64-linux-gnu | grep appindicator   # 5 файлов
$ lumepeer-desktop
  (lumepeer-desktop:584): libayatana-appindicator-WARNING **: ... is deprecated
  exit=124 (жив, убит таймаутом)

# 2. apt-get remove -y libayatana-appindicator3-1  → (no indicator library)
#    2a. СТАРЫЙ бинарник (HEAD до правки):
$ /root/lp-old
  thread 'main' panicked at libappindicator-sys-0.9.0/src/lib.rs:41:5:
  Failed to load ayatana-appindicator3 or appindicator3 dynamic library
  exit=101              # ровно лог пользователя

#    2b. НОВЫЙ бинарник:
$ lumepeer-desktop
  ERROR lumepeer_desktop: no ayatana-appindicator library on this machine,
  so there will be no tray icon and closing the window will quit lumepeer
  instead of hiding it; install libayatana-appindicator3-1 (Debian/Ubuntu)
  or libayatana-appindicator-gtk3 (Fedora) to get the tray back
  exit=124 (жив)

# 3. apt-get install -y libayatana-appindicator3-1 → трей снова на месте
```

Крестик (настоящий `WM_DELETE_WINDOW`, как его шлёт WM — `xdotool
windowclose` это `XDestroyWindow` и до обработчика не доходит, а у WSLg нет
`_NET_CLIENT_LIST` для `wmctrl`):

| | трей есть | библиотеки нет |
|---|---|---|
| процесс после крестика | **жив** | **вышел** |
| видимое окно после крестика | нет (спрятано) | нет |

Автозапуск, первый запуск и «выключено остаётся выключенным»:

```
Linux (свой HOME/XDG_CONFIG_HOME):
  до         : нет .desktop, нет маркера
  после №1   : есть ~/.config/autostart/io.insigmo.lumepeer.desktop + маркер
  rm .desktop (пользователь снял галку)
  после №2   : .desktop НЕ вернулся, маркер на месте

Windows (эта машина, APPDATA в песочницу):
  до         : HKCU\…\Run\Lumepeer отсутствует   ← сам баг
  после №1   : Lumepeer REG_SZ "…\lumepeer-desktop.exe" + маркер
  reg delete (пользователь снял галку)
  после №2   : значение НЕ вернулось
```

Реестр машины возвращён в исходное состояние (значения снова нет), пакеты в
WSL восстановлены, `xdotool`/`wmctrl` удалены.

Имя RPM-пакета сверено с `mdapi.fedoraproject.org`:
`libayatana-appindicator-gtk3` 0.6.0-1.fc46, он же провайдер
`libayatana-appindicator3.so.1`. Расхождение с deb-именем записано в
`docs/platform-support.md`.

## Чего не проверяли

- **Установщик NSIS + перезагрузка Windows целиком.** Проверено то, что
  делает код: первый запуск установленной копии пишет `HKCU\…\Run`. Сам
  цикл «поставить .exe → ребутнуть → приложение поднялось» требует реальной
  установки поверх рабочей и ребута этой машины.
- **macOS.** Код там менялся только в одну сторону — общая часть уехала в
  `Autostart`, путь маркера прежний; Mac в этой сессии не поднимался.
- `cargo fmt --all -- --check` падает на `crates/net/src/dns.rs` — файл
  этой пачкой не трогали, diff был на master до неё (другая версия
  rustfmt схлопывает `#![allow(...)]` в одну строку). Не чинили.
