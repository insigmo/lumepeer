# Пачка 08 — Linux: безопасность

**Репозиторий:** support_knowledge · **Зависит от:** 02 (01 — объём
проверок) · **Нужно:** доступ в веб; ВМ Ubuntu/Fedora/Arch — желательно

Перед началом — `content-workflow.md`, `research/security-assessment.md`,
`contracts.md` § 3. Каталог: `knowledge/linux/security/` (общее) и
`knowledge/linux/families/<family>/security/` (отличия семейств).

## Задачи

### 1. Документ на каждую Linux-проверку (`security_check`)

Для каждого Linux id из `contracts.md` § 3 — общий документ + `variant`
семейства, где сигнал читается по-разному. Особо:

- `linux.update.pending_security` — **это главный сигнал уязвимости на
  Linux.** Как получить список именно обновлений безопасности без прав
  root и без изменения системы:
  - debian/ubuntu: симуляция `apt-get -s dist-upgrade` и происхождение из
    `*-security`; Ubuntu Pro `pro security-status` — если установлен;
  - fedora/RHEL-семейство: `dnf updateinfo`/`dnf advisory` (dnf4 vs dnf5 —
    проверить актуальные команды);
  - suse: `zypper list-patches --category security` (Leap) и что делать на
    Tumbleweed;
  - arch: `checkupdates` (pacman-contrib) + `arch-audit`, если установлен;
    без них — честно `unknown`.
  Почему дистрибутив сам учитывает backport-исправления и почему поэтому
  нельзя сравнивать upstream-версию пакета с CVE.
- `linux.update.metadata_age` — откуда берётся возраст метаданных
  (`/var/lib/apt/lists`, кэш dnf/zypper, pacman sync db) и почему старые
  метаданные дают `unknown`, а не «всё обновлено».
- `linux.firewall.active` — ufw/firewalld/nftables; многое требует root →
  `not_checked (needs elevation)` с объяснением; «нет файрвола» на
  ноутбуке за NAT ≠ critical — severity с учётом слушающих портов.
- `linux.ssh.*` — только если `sshd` установлен и слушает; читать
  `sshd_config` и `sshd_config.d/*` (порядок включения важен), `sshd -T`
  только с правами.
- `linux.disk.encryption` — LUKS под корнем по `lsblk`; отсутствие
  шифрования — рекомендация с честной ценой (переустановка), не «исправить
  в один клик».
- `linux.os.support_status` — EOL релиза по эталону (пачка 13).

### 2. Угрозы и инциденты

1. Подозрение на взлом/вредонос на настольном Linux — что проверить
   безопасно (незнакомые пользователи/ключи SSH, автозапуск, cron/systemd
   timers, слушающие порты), что не делать (не удалять «подозрительное»
   вслепую); когда переустановка честнее.
2. Сторонние репозитории и PPA как риск цепочки поставок — как оценить и
   убрать обратимо.
3. Резервное копирование для дома: Timeshift/snapper (система) vs
   Déjà Dup/Pika (данные) — что от чего защищает.

## Чего НЕ делать

- Не расширять список проверок; не описывать CIS-бенчмарки, Lynis-аудит,
  hardening-гайды целиком (расширенный аудит — пачка 23).
- Не предлагать отключение SELinux/AppArmor, `iptables -F`, открытие
  портов.
- Не сопоставлять CVE по upstream-версиям пакетов.
- Не писать о серверном hardening (fail2ban, веб-серверы и т. п.).

## Definition of done

- [ ] Каждый Linux check id покрыт (проверка покрытия в линтере зелёная).
- [ ] Для `linux.update.pending_security` — проверенные команды для каждого
      семейства P1 (в ВМ, если доступна; иначе — официальная документация
      и `partially_verified`).
- [ ] Документы раздела 2 или записи в `gaps.md`; таблица решений.
