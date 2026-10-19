# Пачка 06 — Linux: общее и семейства (загрузка, systemd, пакеты, сеть, диски)

**Репозиторий:** support_knowledge · **Зависит от:** 02 · **Нужно:** доступ в
веб; ВМ Ubuntu/Debian/Fedora/Arch — желательно

Перед началом — `content-workflow.md` и приоритетная матрица из
`research/os-and-distro-popularity.md`. Каталоги:
`knowledge/linux/common/<area>/` — одинаково на всех;
`knowledge/linux/families/{debian,fedora,arch,suse}/<area>/` — отличия
семейства; `knowledge/linux/distros/<distro>/` — только реальная специфика
дистрибутива (Ubuntu-snap, Mint, Pop!_OS, Bazzite/SteamOS immutable и т. п.).

Главное правило пачки: **одна общая инструкция + дополнения семейств**, а не
копия на каждый дистрибутив. Пример: `linux.boot.initramfs-rebuild` — общий
документ «что такое и когда»; команды `update-initramfs` / `dracut` /
`mkinitcpio` — в `variant`-документах семейств.

## Темы (по убыванию приоритета)

### Загрузка (`boot`)

1. Приглашение `grub>` / `grub rescue>`, «no such partition» —
   диагностика, загрузка вручную, переустановка GRUB (по семействам; с
   рисками для двойной загрузки и Secure Boot).
2. Emergency/maintenance mode из-за `/etc/fstab` (неверный UUID, снятый
   диск) — `nofail`, проверка `findmnt --verify` с резервной копией fstab.
3. Не грузится после обновления ядра — выбрать предыдущее ядро в GRUB,
   пересобрать initramfs, вернуть/удалить проблемное ядро.
4. Файловая система смонтирована только для чтения / ошибки ext4/btrfs —
   признаки отказа диска, `fsck` только на размонтированной ФС.
5. Долгая загрузка — `systemd-analyze blame/critical-chain`, «A start job is
   running for…».

### Службы и журнал (`services`)

6. Служба не запускается — `systemctl status`, `journalctl -u`, частые
   причины (конфиг, права, порт занят); пользовательские юниты.
7. Как читать журнал при проблеме (справочный: `journalctl -b -1 -p err`,
   постоянный журнал, лимиты).

### Пакеты (`packages`) — по семействам

`linux.packages.dpkg-interrupted` уже есть (02).

8. debian: «Could not get lock /var/lib/dpkg/lock» — найти владельца, а не
   удалять lock-файлы.
9. debian: неразрешённые зависимости / «held broken packages» / сторонние
   PPA — `apt-cache policy`, понижение версии, удаление PPA (`ppa-purge`).
10. debian: `NO_PUBKEY` / `apt-key is deprecated` — ключи в
    `/etc/apt/keyrings` + `signed-by`.
11. debian: обновление релиза (Ubuntu `do-release-upgrade`, Debian смена
    suite) сорвалось на середине.
12. fedora: конфликты dnf, `dnf history` и `history undo` (что откатывает и
    что нет), повреждённая RPM DB.
13. arch: частичное обновление (`-Sy` без `u`) — почему опасно и как
    восстановиться; ошибки ключей (`archlinux-keyring`); «exists in
    filesystem»; блокировка `db.lck`.
14. suse: конфликты zypper; Tumbleweed `zypper dup` vs `up`; snapper
    rollback — справочно (подробно — пачка 18).
15. Immutable-системы (Fedora Atomic/Bazzite, SteamOS): `rpm-ostree
    rollback`, почему обычные советы не работают — одним документом
    `distros/` на семейство подходов, если матрица 01 ставит их в P1/P2.

### Сеть (`network`)

16. NetworkManager не подключается — `nmcli`, журнал, конфликт с другими
    менеджерами (dhcpcd, systemd-networkd).
17. DNS не работает — `systemd-resolved` (`resolvectl status`), ссылка
    `/etc/resolv.conf`, DNS от VPN; без захардкоженных `chattr +i`.
18. Нет сети после обновления/смены ядра — модуль драйвера (детали Wi-Fi —
    пачка 07).

### Диски и место (`storage`)

19. Кончилось место — по порядку: `du`/`ncdu`, журнал (`journalctl
    --vacuum-*`), кэши пакетов по семействам, старые ядра, ревизии snap,
    Flatpak unused, Docker; чего не удалять.
20. Кончились inode / btrfs «No space left» при свободном `df` —
    `btrfs filesystem usage`, balance с рисками.
21. Диск Windows (NTFS) не монтируется или только для чтения — быстрый
    запуск/гибернация Windows, `ntfs3` vs `ntfs-3g`; не «чинить» NTFS из
    Linux.

### Права и учётные записи (`accounts`)

22. `sudo` не работает: пользователь не в `sudo`/`wheel`, сломанный
    sudoers (`visudo`, `pkexec` как путь восстановления).
23. «Permission denied» на своих файлах после `sudo`-запуска программ —
    владелец в домашнем каталоге; без `chmod -R 777`.

### Производительность (`performance`)

24. Система тормозит: load, iowait, своп, OOM killer (`journalctl -k`),
    индексаторы (tracker, baloo) — диагностика по `top`/`ps`/`vmstat`.

### Время

25. Неверное время/NTP — `timedatectl`, двойная загрузка с Windows (RTC в
    localtime).

## Чего НЕ делать

- Не писать о звуке, видео, Bluetooth, Wi-Fi-драйверах, печати,
  Flatpak/Snap-приложениях — это 07; безопасность — 08.
- Не делать отдельный документ на каждый дистрибутив, если команды
  совпадают с семейством.
- Не советовать `rm` lock-файлов, `--force-*` dpkg без объяснения,
  `pacman -Sy <pkg>`, `chmod 777`, отключение SELinux/AppArmor как
  решение.
- Не покрывать серверные темы (веб-серверы, БД, Kubernetes) — база для
  настольных ПК.

## Definition of done

- [ ] Документы или записи в `gaps.md`; линтер и дубликаты зелёные.
- [ ] В таблице PR видно, какие документы общие, какие `variant` семейств
      и почему вариант понадобился.
- [ ] Для каждого семейства P1 есть пакетный плейбук (или пробел с
      причиной).
