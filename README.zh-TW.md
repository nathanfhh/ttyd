![backend](https://github.com/tsl0922/ttyd/workflows/backend/badge.svg)
![frontend](https://github.com/tsl0922/ttyd/workflows/frontend/badge.svg)
[![GitHub Releases](https://img.shields.io/github/downloads/tsl0922/ttyd/total)](https://github.com/tsl0922/ttyd/releases)
[![Docker Pulls](https://img.shields.io/docker/pulls/tsl0922/ttyd)](https://hub.docker.com/r/tsl0922/ttyd)
[![Packaging status](https://repology.org/badge/tiny-repos/ttyd.svg)](https://repology.org/project/ttyd/versions)
![GitHub](https://img.shields.io/github/license/tsl0922/ttyd)

# ttyd - 在網頁上分享你的終端

**繁體中文** | [English](README.md)

ttyd 是一個把終端分享到網頁上的簡單命令列工具。

## 這個 fork 增加了什麼

這個 fork 保留上游 C 實作，並在 [`rust/`](rust) 提供相容的 Rust 伺服器移植版。它維持
相同的命令列、HTTP 與 `tty` WebSocket 介面，同時加入 forward authentication
（`--auth-url`），以及可避免將完整命令列傳給 client 的伺服器端標題覆寫（`--title`）。

相容性不是口頭宣稱：一套包含 108 項檢查的差異測試會對兩個版本執行，並在重寫過程中
找出四個真實的行為差異。詳情請見 [相容性報告](rust/PARITY.zh-TW.md)與
[英文原文](rust/PARITY.md)。Rust 版尚未支援 Windows；該平台請使用 C 版。這份實作源自
三十天連載 [《AI 的駕馭之道》](https://ithelp.ithome.com.tw/users/20183518/ironman/9187)。

![screenshot](https://github.com/tsl0922/ttyd/raw/main/screenshot.gif)

# 功能

- 建構於 [libuv](https://libuv.org) 與 [WebGL2](https://developer.mozilla.org/en-US/docs/Web/API/WebGL_API) 之上，追求速度
- 功能完整的終端，支援 [CJK](https://en.wikipedia.org/wiki/CJK_characters) 與輸入法
- 支援 [ZMODEM](https://en.wikipedia.org/wiki/ZMODEM)（[lrzsz](https://ohse.de/uwe/software/lrzsz.html)）/ [trzsz](https://trzsz.github.io) 檔案傳輸
- 支援 [Sixel](https://en.wikipedia.org/wiki/Sixel) 圖片輸出（[img2sixel](https://saitoha.github.io/libsixel) / [lsix](https://github.com/hackerb9/lsix)）
- 基於 [OpenSSL](https://www.openssl.org) / [Mbed TLS](https://github.com/Mbed-TLS/mbedtls) 的 SSL 支援
- 可搭配選項執行任意自訂命令
- 支援 basic authentication 以及許多其他自訂選項
- 跨平台：macOS、Linux、FreeBSD/OpenBSD、[OpenWrt](https://openwrt.org)、Windows

# 安裝

## 在 macOS 上安裝

- 使用 [Homebrew](http://brew.sh)：`brew install ttyd`
- 使用 [MacPorts](https://www.macports.org)：`sudo port install ttyd`

## 在 Linux 上安裝

- Debian/Ubuntu：`sudo apt install ttyd`
- 安裝 snap：`sudo snap install ttyd --classic`
- OpenWrt：`opkg install ttyd`
- Gentoo：clone 這個 [repo](https://bitbucket.org/mgpagano/ttyd/src/master)，並依照[這裡](https://wiki.gentoo.org/wiki/Custom_repository#Creating_a_local_repository)的說明操作。
- 使用 [Homebrew](https://docs.brew.sh/Homebrew-on-Linux)：`brew install ttyd`
- 預先編譯的靜態 binary：從 [releases](https://github.com/tsl0922/ttyd/releases) 頁面下載

## 在 Windows 上安裝

- Binary 版本（建議）：從 [releases](https://github.com/tsl0922/ttyd/releases) 頁面下載
- 使用 [WinGet](https://github.com/microsoft/winget-cli)：`winget install tsl0922.ttyd`
- 使用 [Scoop](https://scoop.sh/#/apps?q=ttyd&s=2&d=1&o=true)：`scoop install ttyd`
- [在 Windows 上編譯](https://github.com/tsl0922/ttyd/wiki/Compile-on-Windows)

# 使用方式

## 命令列選項

以下為 `ttyd --help` 的原始輸出，維持英文原文，以免與 binary 實際印出的內容不符。

```
USAGE:
    ttyd [options] <command> [<arguments...>]

OPTIONS:
    -p, --port              Port to listen (default: 7681, use `0` for random port)
    -i, --interface         Network interface to bind (eg: eth0), or UNIX domain socket path (eg: /var/run/ttyd.sock)
    -U, --socket-owner      User owner of the UNIX domain socket file, when enabled (eg: user:group)
    -c, --credential        Credential for basic authentication (format: username:password)
    -H, --auth-header       HTTP Header name for auth proxy, this will configure ttyd to let a HTTP reverse proxy handle authentication
    -u, --uid               User id to run with
    -g, --gid               Group id to run with
    -s, --signal            Signal to send to the command when exit it (default: 1, SIGHUP)
    -w, --cwd               Working directory to be set for the child program
    -a, --url-arg           Allow client to send command line arguments in URL (eg: http://localhost:7681?arg=foo&arg=bar)
    -W, --writable          Allow clients to write to the TTY (readonly by default)
    -t, --client-option     Send option to client (format: key=value), repeat to add more options
    -T, --terminal-type     Terminal type to report, default: xterm-256color
    -O, --check-origin      Do not allow websocket connection from different origin
    -m, --max-clients       Maximum clients to support (default: 0, no limit)
    -o, --once              Accept only one client and exit on disconnection
    -q, --exit-no-conn      Exit on all clients disconnection
    -B, --browser           Open terminal with the default system browser
    -I, --index             Custom index.html path
    -b, --base-path         Expected base path for requests coming from a reverse proxy (eg: /mounted/here, max length: 128)
    -P, --ping-interval     Websocket ping interval(sec) (default: 5)
    -6, --ipv6              Enable IPv6 support
    -S, --ssl               Enable SSL
    -C, --ssl-cert          SSL certificate file path
    -K, --ssl-key           SSL key file path
    -A, --ssl-ca            SSL CA file path for client certificate verification
    -d, --debug             Set log level (default: 7)
    -v, --version           Print the version and exit
    -h, --help              Print this text and exit
```

使用範例請見 [wiki](https://github.com/tsl0922/ttyd/wiki/Example-Usage)。

# Rust 實作

這個 fork 另外在 [`rust/`](rust) 底下包含一份伺服器的 Rust 移植版。它的命令列、HTTP 介面與 `tty` WebSocket 協定都與 C 版相容——兩者嵌入的是來自 `src/html.h` 的同一份前端 bundle——並加入兩項 C 版無法解決的需求：

- **Forward authentication**（`--auth-url`）：每一個請求，包含 WebSocket upgrade，都會先向一個外部端點驗證，做法與 nginx 的 `auth_request` 及 Traefik 的 ForwardAuth middleware 相同。這讓既有的 SSO 或身分服務可以保護終端，而不是只靠一組共用的 basic-auth 憑證。
- **`--title`**：取代 window title，否則它會把完整命令列送給每一個開啟 session 的 client。

## 建置 Rust 實作

只需要 [Rust toolchain](https://rustup.rs)，不需要 libwebsockets、libuv 或 json-c：

```bash
cd rust
cargo build --release
./target/release/ttyd --help
# binary 位於 rust/target/release/ttyd
```

執行方式：

```bash
# 與 C 版相同的選項
./target/release/ttyd -W bash

# 由外部認證服務保護，並隱藏命令列
./target/release/ttyd \
    --auth-url https://sso.internal/verify \
    --title "Support Console" \
    -W bash
```

移植版透過差異測試套件對照 C 版驗證，同一組斷言會對兩顆 binary 各執行一次：

```bash
cd rust
cargo test                                 # 全部，對這個版本執行
./run-parity-tests.sh /path/to/c/ttyd      # 再對 C 版執行一次並比對
```

完整選項參考請見 [rust/README.zh-TW.md](rust/README.zh-TW.md)，與 C 版比較的結果請見 [rust/PARITY.zh-TW.md](rust/PARITY.zh-TW.md)。

請注意 Rust 移植版不涵蓋 Windows；在該平台請使用 C 版。Unix 路徑在 Linux 與 macOS 上都經過實測。

## 瀏覽器支援

現代瀏覽器，詳見 [Browser Support](https://github.com/xtermjs/xterm.js#browser-support)。

## 其他選擇

* [Wetty](https://github.com/krishnasrinivas/wetty)：基於 [Node](https://nodejs.org) 的網頁終端（SSH/login）
* [GoTTY](https://github.com/yudai/gotty)：基於 [Go](https://golang.org) 的網頁終端
