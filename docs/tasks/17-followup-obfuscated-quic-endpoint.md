# 17-followup — Обфусцированный QUIC-эндпоинт (increment 1)

Это продолжение `17-serverless-obfuscated-quic.md`. Инкремент 1 из «дорожной
карты» ADR 0052. Пиши весь код, комментарии, имена и коммиты **на английском**;
прозу задачи можно на русском.

---

## 0. Что уже сделано — НЕ перепроверять, НЕ переспрашивать

- **Fase 0 выполнена** (см. `17-serverless-obfuscated-quic.md`, раздел
  «Результат Fase 0»). Не перезапускай сетевые замеры. Не пытайся соединить
  пару машин через провайдера — это отдельный шаг валидации, не этот.
- **Архитектура решена** — `docs/adr/0052-serverless-transport-obfuscated-quic-with-stun-discovery.md`:
  прямой обфусцированный QUIC, данные никогда не идут через сервер;
  обнаружение адреса — свой STUN (не n0). Не переигрывай это.
- **Уже в репозитории и зелёное** (не переписывай, используй как есть):
  - `crates/net/src/obfuscate.rs` — кодек. API: `Obfuscator::for_host(&invite_id)`
    / `Obfuscator::for_guest(&invite_id)` где `invite_id: &[u8; INVITE_ID_BYTES]`
    (из `crate::ticket::INVITE_ID_BYTES`); методы `seal(&[u8]) -> Result<Vec<u8>>`
    и `open(&[u8]) -> Result<Vec<u8>>`. `open` на плохом вводе возвращает
    `Err(NetError::Obfuscation)` без паники.
  - `crates/net/src/stun.rs` — обнаружение адреса (`reflexive_addr`). Живьём
    проверено.
  - Константы `OBFUSCATE_PADDING_MAX_BYTES`, `STUN_QUERY_TIMEOUT_MS` в
    `crates/core/src/constants.rs`. Вариант ошибки `NetError::Obfuscation` в
    `crates/net/src/error.rs`.
- **iroh 1.0.2 НЕ даёт хук на свой UDP-сокет.** Не пытайся обернуть сокет
  внутри `iroh::Endpoint`. Точка встраивания — `noq` напрямую (ниже).

## 1. Обязательно прочитать перед стартом

1. `docs/tasks/README.md` — «Обязательно к прочтению» и «Правила, нарушение
   которых заворачивает работу».
2. `docs/adr/0051-*.md` и `docs/adr/0052-*.md`.
3. Файлы: `crates/net/src/obfuscate.rs`, `crates/net/src/stun.rs`,
   `crates/net/src/error.rs`, `crates/net/src/keystore.rs` (идиома крипты),
   `crates/core/src/constants.rs`.

## 2. Жёсткие правила (нарушение = переделка)

- **Никаких магических чисел.** Любая новая числовая величина — `pub const` в
  `crates/core/src/constants.rs` с doc-комментарием и ссылкой на ADR 0052.
- **Никаких паник на недоверенном вводе** в `crates/net`. Датаграмма из сети —
  недоверенная. Ни `unwrap`/`expect`/`panic!`/индексации вне проверки на
  путях приёма. Возвращай `NetError`. (Кодек `open` уже такой — опирайся на
  него.)
- **clippy pedantic + `-D warnings`** и `cargo fmt` обязаны проходить.
- **Не удалять существующие пути.** `PeerEndpoint`, iroh, relay — не трогать.
  Новый транспорт добавляется рядом.
- **Обфускация не расширяет грант.** Согласие/гранты остаются в `crates/core`;
  этот код только про транспорт.
- **Отклонение от плана — ADR.** Следующий свободный номер — grep по коду:
  `grep -rho "ADR 0[0-9][0-9][0-9]" crates apps --include=*.rs | sort -u`
  (сейчас максимум 0052) + `ls docs/adr`. Бери первый свободный.

## 3. Задача этой сессии

Построить **обфусцированный QUIC-сокет** — тип, реализующий
`noq::AsyncUdpSocket`, который шифрует каждую исходящую датаграмму кодеком и
расшифровывает входящую, — и **доказать, что настоящий QUIC над ним работает**
двумя локальными эндпоинтами, обменивающимися данными в обе стороны.

**Критерий готовности:**
- `cargo test -p lumepeer-net` зелёный, включая новый тест, где два `noq`-QUIC
  эндпоинта поверх обфусцированного сокета открывают stream и обмениваются
  сообщением в обе стороны.
- `cargo clippy -p lumepeer-core -p lumepeer-net --all-targets -- -D warnings`
  чистый; `cargo fmt --all -- --check` проходит.
- `keep_alive_interval` реально выставлен на `TransportConfig` из новой
  константы (см. §6, ловушка «ложный разрыв ~30с»).

## 4. Точные факты API — `noq` 1.2.0 (проверено по исходникам)

`noq` уже в дереве (через iroh). Добавь `noq` в `[dependencies]` или
`[dev-dependencies]` крейта `crates/net` — версия ровно `1.2.0`, чтобы не
задвоить. Это ADR-момент (ADR 0001: «добавить прямую зависимость на quinn» =
ADR); зафиксируй в ADR 0052 или новом коротком ADR, что `noq` стал прямой
зависимостью.

Всё нужное реэкспортируется из корня `noq`:

- Точка встраивания сокета:
  ```rust
  noq::Endpoint::new_with_abstract_socket(
      config: EndpointConfig,
      server_config: Option<ServerConfig>,
      socket: Box<dyn noq::AsyncUdpSocket>,
      runtime: Arc<dyn noq::Runtime>,
  ) -> io::Result<noq::Endpoint>
  ```
  Есть и попроще: `noq::Endpoint::new(config, server_config, socket: std::net::UdpSocket, runtime)`
  — но нам нужен именно `new_with_abstract_socket`, чтобы подсунуть свой сокет.
- Базовый сокет, который надо обернуть, получается так:
  `runtime.wrap_udp_socket(std_udp: std::net::UdpSocket) -> io::Result<Box<dyn AsyncUdpSocket>>`.
  Рантайм: `noq::TokioRuntime` (или `noq::default_runtime()`), обёрнутый в
  `Arc`. Наш `ObfuscatedSocket` держит этот `Box<dyn AsyncUdpSocket>` как
  `inner` и делегирует ему реальный ввод-вывод, добавляя seal/open.
- Трейт, который реализуем (`noq::AsyncUdpSocket`):
  ```rust
  fn create_sender(&self) -> Pin<Box<dyn UdpSender>>;
  fn poll_recv(&mut self, cx: &mut Context, bufs: &mut [IoSliceMut], meta: &mut [RecvMeta])
      -> Poll<io::Result<usize>>;
  fn local_addr(&self) -> io::Result<SocketAddr>;
  fn max_receive_segments(&self) -> NonZeroUsize { NonZeroUsize::MIN } // ВЕРНУТЬ 1 — см. ловушку GRO
  fn may_fragment(&self) -> bool { true } // делегировать inner
  ```
- Трейт отправителя (`noq::UdpSender`):
  ```rust
  fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, cx: &mut Context) -> Poll<io::Result<()>>;
  fn max_transmit_segments(&self) -> NonZeroUsize { NonZeroUsize::MIN } // ВЕРНУТЬ 1 — отключить GSO
  ```
- `Transmit` (из `noq::udp::Transmit`, реэкспорт `noq::Transmit`):
  ```rust
  pub struct Transmit<'a> {
      pub destination: SocketAddr,
      pub ecn: Option<EcnCodepoint>,
      pub contents: &'a [u8],
      pub segment_size: Option<usize>, // при max_transmit_segments()==1 всегда None
      pub src_ip: Option<IpAddr>,
  }
  ```
- `RecvMeta` (из `noq::udp::RecvMeta`): `{ addr: SocketAddr, len: usize, stride: usize, .. }`.
  `stride` — размер одной датаграммы при GRO; если буфер длиннее `stride`,
  внутри несколько датаграмм на границах кратных `stride`.
- Конфиги/крипто (реэкспорт из `noq`): `EndpointConfig`, `ServerConfig`,
  `ClientConfig`, `TransportConfig`, `IdleTimeout`, `VarInt`, `crypto`,
  `rustls`, `TokioRuntime`, `default_runtime`, `Connection`.

## 5. Что построить (в `crates/net/src/obfuscate.rs`, рядом с кодеком)

### 5.1 `ObfuscatedSocket`
```rust
pub struct ObfuscatedSocket {
    inner: Box<dyn noq::AsyncUdpSocket>,
    obfuscator: Obfuscator,
}
impl ObfuscatedSocket {
    pub fn new(inner: Box<dyn noq::AsyncUdpSocket>, obfuscator: Obfuscator) -> Self { .. }
}
impl std::fmt::Debug for ObfuscatedSocket { /* без ключей */ }
impl noq::AsyncUdpSocket for ObfuscatedSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(ObfuscatedSender { inner: self.inner.create_sender(), obfuscator: self.obfuscator.clone() })
    }
    fn poll_recv(&mut self, cx, bufs, meta) -> Poll<io::Result<usize>> { /* см. 5.3 */ }
    fn local_addr(&self) -> io::Result<SocketAddr> { self.inner.local_addr() }
    fn max_receive_segments(&self) -> NonZeroUsize { NonZeroUsize::MIN } // 1: не коалесить GRO
    fn may_fragment(&self) -> bool { self.inner.may_fragment() }
}
```

### 5.2 `ObfuscatedSender` (отправка)
```rust
struct ObfuscatedSender { inner: Pin<Box<dyn UdpSender>>, obfuscator: Obfuscator }
impl UdpSender for ObfuscatedSender {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit, cx) -> Poll<io::Result<()>> {
        // GSO отключён (max_transmit_segments()==1) => transmit.contents = одна датаграмма.
        let this = self.get_mut(); // Pin<Box<dyn UdpSender>> — Unpin, get_mut ок
        let sealed = this.obfuscator.seal(transmit.contents)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let out = Transmit {
            destination: transmit.destination,
            ecn: None,               // паддинг меняет длину; ECN на обёртке не несём
            contents: &sealed,       // sealed живёт до конца вызова — ок
            segment_size: None,
            src_ip: transmit.src_ip,
        };
        this.inner.as_mut().poll_send(&out, cx)
    }
    fn max_transmit_segments(&self) -> NonZeroUsize { NonZeroUsize::MIN } // отключить GSO
}
```

### 5.3 `poll_recv` (приём — самое тонкое место)
Логика: принять шифртекст во временные буферы через `inner.poll_recv`,
расшифровать каждую датаграмму кодеком, записать открытый текст в `bufs`
вызывающего и заполнить `meta`. Плохую датаграмму (`open` вернул `Err`) —
**молча выбросить**, не роняя соединение.

```rust
loop {
    // временные буферы под шифртекст (по числу bufs)
    let mut tmp = vec![[0u8; 2048]; bufs.len()];
    let mut tmp_io: Vec<IoSliceMut> = tmp.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut tmp_meta = vec![RecvMeta::default(); bufs.len()];
    match self.inner.poll_recv(cx, &mut tmp_io, &mut tmp_meta)? {
        Poll::Pending => return Poll::Pending,
        Poll::Ready(n) => {
            let mut out = 0;
            for i in 0..n {
                // ВАЖНО: одна запись RecvMeta может нести несколько датаграмм по stride.
                // Пройти датаграммы срезами stride (последняя короче) и открыть каждую.
                for datagram in split_by_stride(&tmp[i], tmp_meta[i].len, tmp_meta[i].stride) {
                    if let Ok(plain) = self.obfuscator.open(datagram) {
                        // записать plain в bufs[out], заполнить meta[out]
                        // (проверить, что out < bufs.len(); если места нет — оставить на след. poll)
                        write_into(bufs, meta, &mut out, tmp_meta[i].addr, &plain);
                    } // else: молча пропустить
                }
            }
            if out == 0 { continue; } // всё был мусор — снова poll, а не Ready(0)
            return Poll::Ready(Ok(out));
        }
    }
}
```
Замечания:
- Мы вернули `max_receive_segments()==1`, поэтому `noq` даёт нам буферы под одну
  датаграмму; но `inner` мог коалесцировать по GRO, значит `split_by_stride`
  нужен для честности. Если хочешь упростить и уверен, что GRO выключен —
  можно обрабатывать только целиком `tmp[i][..len]`, но тогда добавь коммент,
  что рассчитываешь на отсутствие GRO. Референс обработки stride —
  `noq-udp-1.2.0/src/lib.rs` (`RecvMeta`) и как iroh читает приём в
  `iroh-1.0.2/src/socket/transports/ip.rs`.
- `RecvMeta` для открытого текста: `addr` из `tmp_meta[i].addr`, `len =
  plain.len()`, `stride = plain.len()`.
- Пустой ввод/битая датаграмма не должны паниковать — кодек это уже
  гарантирует; просто пропускай `Err`.

### 5.4 Хелпер конфигурации транспорта
Функция, собирающая `noq::TransportConfig` с обязательным keep-alive:
```rust
pub fn obfuscated_transport_config() -> noq::TransportConfig {
    let mut cfg = noq::TransportConfig::default();
    cfg.keep_alive_interval(Some(Duration::from_secs(QUIC_KEEPALIVE_SECS)));
    cfg.max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_secs(QUIC_MAX_IDLE_TIMEOUT_SECS)).unwrap()));
    cfg
}
```
(`unwrap` здесь на константе, не на сетевом вводе — допустимо, но лучше
`expect` с текстом или собрать `IdleTimeout` из `VarInt`. Проверь, что clippy
не ругается; при необходимости вынеси в отдельную функцию с `# Panics`-доком.)

## 6. Константы (в `crates/core/src/constants.rs`)

```rust
/// Keep-alive interval on the obfuscated QUIC transport (task 17, ADR 0052).
/// Mandatory: without a keep-alive the QUIC idle timeout closes an otherwise
/// healthy path at ~30 s, which was once mistaken for a DPI drop
/// (project-lumepeer-quic-vs-relay-transport). Must stay below
/// [`QUIC_MAX_IDLE_TIMEOUT_SECS`].
pub const QUIC_KEEPALIVE_SECS: u64 = 15;

/// Idle timeout on the obfuscated QUIC transport (task 17, ADR 0052). Larger
/// than twice [`QUIC_KEEPALIVE_SECS`] so a single lost keep-alive never trips
/// it, but bounded so a truly dead path is eventually released.
pub const QUIC_MAX_IDLE_TIMEOUT_SECS: u64 = 60;
```
(Порт по умолчанию — не 443 — на этом шаге НЕ нужен: биндись на `0.0.0.0:0`,
ОС даст случайный высокий порт. Смена порта и не-443 дефолт — increment 2/3.)

## 7. Тесты (Fase 4)

1. **Обязательный — уровень сокета.** Два `ObfuscatedSocket` (host/guest) поверх
   двух `tokio`/`std` UDP-сокетов на `127.0.0.1:0`. Через `create_sender()`
   отправить `Transmit`, через `poll_recv` принять; проверить, что открытый
   текст совпал в обе стороны, и что на проводе (перехватив на «голом» inner)
   байты обфусцированы (не равны открытому тексту). Это доказывает обёртку без
   TLS.
2. **Рекомендуемый — полный QUIC.** Два `noq::Endpoint` через
   `new_with_abstract_socket` поверх `ObfuscatedSocket`, с
   `obfuscated_transport_config()`. Клиент коннектится к серверу, открывает
   bi-stream, шлёт сообщение, сервер отвечает; проверить обе стороны. Для
   сертификатов проще всего повторить паттерн из тестов самого `noq`:
   смотри `~/.cargo/registry/src/index.crates.io-*/noq-1.2.0/src/tests.rs`
   (там есть сборка пары client/server endpoint с self-signed cert). Если
   нужен `rcgen` — добавь его в `[dev-dependencies]` крейта `net`.
   Оба эндпоинта используют один и тот же `invite_id`, один
   `Obfuscator::for_host`, другой `Obfuscator::for_guest`.

Тесты крипто-round-trip уже есть в `obfuscate.rs` — не дублируй, добавляй
только сокет/эндпоинт.

## 8. Проверка перед завершением

`apps/desktop` НЕ трогается на этом шаге, поэтому пересборка webview не нужна.
```sh
cargo fmt --all -- --check
cargo clippy -p lumepeer-core -p lumepeer-net --all-targets -- -D warnings
cargo test -p lumepeer-core -p lumepeer-net
```
(Полный `cargo test --workspace` на этой машине красный по независимой причине —
установлен LumepeerHelper; см. `project-lumepeer-single-instance-and-service-test`.
Ограничься `-p lumepeer-core -p lumepeer-net`.)

## 9. Дальше (increments 2-4 из ADR 0052 — НЕ в этой сессии)

2. Hole punch + STUN-адрес (`stun::reflexive_addr`) в инвайт-тикет; одновременная
   отправка обеими сторонами по адресам из тикета.
3. Провод каналов приложения (control/media/file) на новый транспорт за
   существующим путём согласия/грантов; выбор транспорта в
   `apps/desktop/src-tauri/src/network.rs`.
4. Валидация на паре (LAN, затем провайдер), Fase 3 (декой-пакет с малым TTL,
   смена порта, адаптивная эскалация), не-443 порт.

## 10. Ловушки

- **Ложный «разрыв ~30с»** — это QUIC idle-timeout, НЕ DPI. Всегда включай
  keep-alive (см. §6). Любой тест «держания» без keep-alive врёт.
- **GSO/GRO ломают переменную длину.** Обфускация делает каждую датаграмму
  разной длины (случайный паддинг), а GSO/GRO рассчитаны на равные сегменты.
  Поэтому `max_transmit_segments()`/`max_receive_segments()` → 1, и обрабатывай
  датаграммы поштучно (см. 5.3 про `stride`).
- **`noq` ровно `1.2.0`** — не дай Cargo подтянуть другую мажор/минор рядом с
  той, что тянет iroh, иначе типы `AsyncUdpSocket` из двух разных `noq` не
  совпадут и `new_with_abstract_socket` не примет твой сокет.
- **Не логируй ключи.** `Debug` у `ObfuscatedSocket`/`ObfuscatedSender` — без
  байтов ключа (как у `Obfuscator`).
