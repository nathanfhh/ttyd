# ttyd — Rust 移植版

**繁體中文** | [English](README.md)

這是 [ttyd](https://github.com/tsl0922/ttyd) 的 Rust 實作，命令列介面、HTTP 介面與 `tty` WebSocket 協定都與 C 原版相容，並加入 **forward authentication**，讓 ttyd 可以交由既有的 SSO 或身分服務保護，而不是只能依賴一組靜態的 basic-auth 憑證。

前端沒有任何改動：建置時讀取的是 C 版嵌入的同一份 `src/html.h`，因此兩者提供逐 byte 相同的 bundle。

## 建置

```sh
cd rust
cargo build --release
# target/release/ttyd
```

唯一的外部需求是 [Rust toolchain](https://rustup.rs)，不需要 libwebsockets、libuv 或 json-c。C 版的每一個命令列選項都被接受，因此既有的啟動指令可以原封不動沿用：

```sh
./target/release/ttyd -W bash
```

## 把命令列藏起來：`--title`

伺服器公布的 window title 預設是完整命令列加上主機名稱，而且每一個開啟 session 的 client 都會收到它。當命令中含有任何敏感內容——script 路徑、主機、金鑰——`--title` 會直接整個取代掉：

```sh
ttyd --title "Support Console" -W /opt/ops/reset-account.sh --token abc123
```

瀏覽器分頁於是只顯示 `Support Console`，命令列則完全不會被送上網路。這與既有的 `-t titleFixed=…` client 選項不同；後者只改變真正的 title 已經送出之後、瀏覽器所顯示的內容。

## Forward authentication

C 版提供兩種保護終端的方式：靜態的 `user:password` 憑證（`--credential`），或無條件信任反向代理設定的某個標頭（`--auth-header`）。兩者都無法與身分提供者整合。Forward auth 以 nginx 的 `auth_request` 和 Traefik 的 ForwardAuth middleware 相同的方式補上這個缺口：每一個請求——包含 WebSocket upgrade——都會先送到一個由你掌控的端點，只有 2xx 的回應才會放行。

以下為 `ttyd --help` 的 FORWARD AUTHENTICATION 區塊原始輸出，維持英文原文：

```text
-R, --auth-url          Delegate authentication to this URL, like nginx `auth_request`.
                        A 2xx response admits the request, anything else rejects it.
-F, --auth-request-header
                        Request header to copy into the auth subrequest, repeat to add
                        more (default: Cookie, Authorization)
-N, --auth-user-header  Auth response header carrying the user name, exposed to the
                        child process as TTYD_USER (default: X-Auth-User)
    --auth-method       HTTP method for the auth subrequest (default: GET)
    --auth-cache-ttl    Seconds to cache an auth decision (default: 0, no caching)
```

```sh
ttyd --auth-url https://sso.internal/verify \
     --auth-request-header Cookie \
     --auth-user-header X-Auth-User \
     --auth-cache-ttl 30 \
     -W bash
```

值得知道的行為：

- Subrequest 會攜帶 `X-Original-Method`、`X-Original-URI`、`X-Forwarded-Method`、`X-Forwarded-Uri`、`X-Forwarded-Proto`、`X-Forwarded-Host` 與 `X-Forwarded-For`，因此為 nginx 或 Traefik 寫的服務可以原封不動運作。
- `X-Forwarded-For` **只帶 ttyd 自己觀察到的位址**。client 送來的 `X-Forwarded-For` 會被丟棄而不是接在後面，因為執行 forward auth 的 ttyd 通常就是邊緣：上游沒有可信任的一跳，所以 client 送來的任何內容都是攻擊者可以決定的。相對地，`X-Forwarded-Host` 依定義就是 client 自己的 `Host` 標頭——在你的 auth 服務裡請把它當成不可信的輸入。
- 非 2xx 的回應會連同 `WWW-Authenticate`、`Proxy-Authenticate`、`Location`、`Set-Cookie` 與 `Cache-Control` 一起轉發給瀏覽器。因此導向登入頁的 `302` 可以完整運作。
- 若端點無法連上，請求會以 `500` 拒絕。認證服務中斷時絕對不會放行任何人。
- 只有成功的判定會被快取。快取拒絕會讓使用者即使已經登入，仍在剩餘的 TTL 內被擋在門外。
- Cache key 由 **subrequest 攜帶的完整輸入集合**衍生——method、URI、被轉發的 request header，以及每一個 `X-Forwarded-*`／`X-Original-*` 的值。凡是你的端點有資格據以判斷的內容都屬於 key 的一部分，因此一項判定不會被重播到端點原本會拒絕的請求上。
- `--auth-url` 的優先權高於 `--credential` 與 `--auth-header`，且 `/token` 端點會停止發放憑證。

值得預先規劃的運作限制：

- 每一個沒有命中快取的請求都會產生一次 subrequest，而拒絕是刻意永不快取的——所以未經認證的流量，也就是攻擊者可以完全掌控的那種，一定會抵達你的身分提供者。若 ttyd 直接暴露在公開網際網路上，請在前面放一台會做 rate limiting 的反向代理。
- 由於端點無法連上時是 fail closed 回 `500`，認證服務中斷會連帶讓終端一起不可用。這是刻意選擇的取捨；請依此規劃端點的容量。

## 測試

移植版透過差異式特徵測試套件對照 C 版驗證。方法、結果與找到的行為差異記錄在 [PARITY.zh-TW.md](PARITY.zh-TW.md)。

```sh
cargo test                                  # 全部，對這個版本執行
./run-parity-tests.sh /path/to/c/ttyd       # 再對 C 版執行一次並比對
```

上述套件以 synthetic client 驅動協定，能證明 wire format，卻不能證明出貨的前端能與之配合。`browser-check.py` 補上這個缺口：它透過 Playwright 在真實 Chromium 中開啟頁面，檢查 xterm.js 能 mount、按鍵能抵達 shell、`TERM` 與視窗縮放能送達、全螢幕程式（`vi`）能完整 round-trip，以及 session 全程不中斷。它也會留下 screenshot，供檢視顏色、CJK 與框線字元的輸出。

```sh
pip install playwright && playwright install chromium
python3 browser-check.py                          # 這個版本
python3 browser-check.py /path/to/c/ttyd c        # 以及 C 版，用於比較
TTYD_BROWSER_TLS=1 python3 browser-check.py       # 同一輪，改走 HTTPS
```

它使用 Playwright 自帶的 Chromium。在已內建固定版本瀏覽器的 image 上，即使系統中已安裝可用的 Chromium，受管理的啟動方式仍可能失敗；此時 script 會退而使用 `PLAYWRIGHT_BROWSERS_PATH` 底下找到的那一份並明確說明。你也可以設定 `TTYD_CHROMIUM=/path/to/chrome` 自行指定 binary。

`browser-check.py` 證明前端能運作，但不能證明終端長時間開著也沒事。`e2e-soak.py` 會在並行負載下維持一個真實瀏覽器 session 二十分鐘，每三十秒要求它執行一個命令並證明 shell 真的執行了，同時全程對伺服器取樣：

```sh
python3 e2e-soak.py                          # 20 分鐘，預設值
python3 e2e-soak.py ./target/release/ttyd 300  # 或跑短一點
```

只要 session 中途停止運作、瀏覽器看到 WebSocket 關閉，或伺服器的 descriptor 或 thread 數量成長，它就會失敗。由於每一次探測都會記錄 shell 自己的 PID，一次無聲的重新連線——那在畫面上看起來完全一樣，因為前端會自行重連——會以 PID 改變的形式暴露出來，而不是悄悄通過。

`bench.py` 產生 `PARITY.zh-TW.md` 裡的效能表格，並將兩個版本交錯執行，讓 machine drift 同時落在兩者身上。若機器上已經有負載產生器在跑，它會拒絕啟動；它也會掃描自己遺留的孤兒程序並回報數量——一個把自己變成負載的 benchmark，正是這個 harness 要讓人看得見的失敗：

```sh
python3 bench.py        # 5 輪，預設值
python3 bench.py 1      # 快速檢查
```

它預期 C 參考版位於 `../build-c/ttyd`；可用 `TTYD_C_BIN` 與 `TTYD_RUST_BIN` 覆寫。

## 供應鏈

```sh
cargo audit                                    # 以 RustSec advisory 檢查 Cargo.lock
cargo cyclonedx --format json --spec-version 1.5   # 產生 ttyd.cdx.json
```

SBOM 採即時產生而非簽入版本庫。它完全衍生自 `Cargo.lock`，而後者**確實**有簽入，因此任何一個 revision 都能精確重現自己的 SBOM；相對地，存起來的副本會在下一次相依套件更新時默默過期，而過期的 SBOM 比沒有更糟，因為它會被信任。請在 CI、發布時或需要時再產生。

2026-07-26 UTC 對 1169 則 RustSec advisory 執行的結果：從 24 個直接相依鎖定出 222 個 crate，**當時沒有已知漏洞**，另有一項 informational warning——`rustls-pemfile` 已無人維護（RUSTSEC-2025-0134）。請重新執行上述指令，而不要信任這一段文字——「沒有漏洞」是關於某個日期的陳述，不是程式碼的性質。該套件用來解析 `--ssl-cert`、`--ssl-key` 與 `--ssl-ca` 指定的 PEM 檔案，那些是 operator 提供的本機檔案，而非網路輸入。

## 相容性

C 實作的每一個命令列選項、HTTP endpoint、WebSocket 訊息型別與認證模式都有支援。有三項行為刻意不同：非正常結束時不會把保留碼 1006 送上 wire、`PAUSE` 真的會暫停，以及 basic-auth 比較採用 constant time，每一項都在 [PARITY.zh-TW.md](PARITY.zh-TW.md) 中說明。`--title`、`--auth-url` 及其搭配選項是新增的；既有的功能沒有移除任何一項。

**Windows 尚未移植。**C 版透過 ConPTY 支援它；這個移植版只實作 Unix PTY 路徑。Windows backend 可以放在相同的 `pty` module interface 後方，這次選擇不提供，而不是交付未經測試的實作。

**Unix 路徑在 Linux 與 macOS 上都經過實測**，兩邊的測試套件都全綠。BSD 系統未經測試。

## 版本編號

這個移植版延續 C 專案的版本線，而不是從 `0.1.0` 重新開始。對使用者而言它就是同一個程式——同樣的選項、同樣的 wire protocol、同樣的前端 bundle——所以重新編號既無法提供操作人員有用的資訊，也會失去「這一版接在 1.7.7 之後」這個事實。

**它從 2.0.0 起算。**不是 1.8.0：即使每一項可觀察行為都被保留，用另一種語言重新實作仍是使用者能被交付的最大變更；而且平台支援確實變窄了，因為 C 版可以在 Windows 上執行，這一版不行。依 semver，光是少支援一個平台就已構成破壞性變更。

因此 `--version` 回報 `2.0.1-<short git hash>`，而 C 版回報 `1.7.7-<short git hash>`。格式刻意相同，數字刻意不同：在野外撿到的一顆 binary，絕不該讓人分不清它是哪一個實作。版本號不會在兩個版本之間流通，測試套件裡也沒有任何東西在比較它們——差異測試比較的是**行為**，那才是應該一致的東西。

Release 以純版本號打 tag，與 C 專案自己的 tag 方式相同。
