# 相容性報告：C 實作與 Rust 移植版

**繁體中文** | [English](PARITY.md)

本文記錄 Rust 移植版如何對照原始 C 實作進行驗證、驗證發現了什麼，以及兩者在哪些地方刻意採取不同做法。

## 方法

移植版使用**特徵測試**（characterization testing，也稱 golden-master testing）驗證：測試套件不主張程式碼「應該」怎麼運作，而是記錄既有實作「實際」怎麼運作，重寫版必須重現這些行為。

同時搭配**差異測試**（differential testing）：同一套測試透過 `TTYD_BIN` 環境變數選擇 binary，分別對兩個版本執行。若一項測試在其中一版通過、另一版失敗，就代表存在必須解釋的行為差異——可能是移植版寫錯，也可能是原版有值得刻意修正的缺陷。

```sh
./run-parity-tests.sh /path/to/c/ttyd
```

這兩種方法都不能證明兩個實作完全等價。為了界定測試套件漏掉的範圍，C 版也以 `--coverage` 編譯並執行同一套測試：任何沒有被跑到的 C 程式碼，代表其行為未受檢查。下方會列出這個數字；覆蓋率暴露的缺口也用來補寫測試，直到剩下的只有無法觸發的錯誤路徑。

## 結果

覆蓋率刻意使用三種不同的分母呈現，因為只給一個數字容易誘使讀者做出方法本身不支持的比較。前兩列才是可直接對照的組合：同一組 108 支共用測試，分別對兩個版本執行。

| 測量對象 | 範圍 | 行覆蓋率 |
|---|---|---|
| C 參考版（`src/*.c`，966 行） | 108 支共用相容性測試 | **88.72 %** |
| Rust 移植版（`rust/src/*.rs`） | 同一組 108 支共用相容性測試 | **80.58 %** |
| Rust 移植版，不含 `auth.rs` | 同一組 108 支共用相容性測試 | **86.55 %** |
| Rust 移植版（2778 行） | 整套測試，含單元測試與 forward-auth | **93.82 %** |

可以互相比較的是前兩列，而結果並不偏袒這個移植版：共用測試觸及的 Rust 程式碼比例**低於** C 程式碼。差距大多來自 `auth.rs`，也就是 forward authentication；C 版沒有這項功能，因此共用測試本來就不可能跑到它。這部分改由 19 支沒有 C 版對照組的 `forward_auth` 測試覆蓋。排除 `auth.rs` 後，差距縮小至約兩個百分點。其餘差距則是因為以同一套計數方式看，移植版的程式碼量接近原版的三倍——2778 行對 966 行——其中一部分是 C 版沒有對應內容的錯誤處理。若改以原始行數計算，比例較小，是 4628 行對 1965 行。

最後一列適合回答「這個移植版整體有多少程式碼至少被測過」，但**不應**拿來與 C 版數字並列比較。

這四個數字量測於 `bfa0d05`，當時測試套件是 221 支、移植版是 4409 行；之後套件多了一支單元測試，程式碼也長到 4628 行。C 版的數字不受影響，因為 `src/*.c` 自 fork 以來沒有變動。下方清單是目前的計數，不是產生上述百分比的那一組。

測試清單：

| 測試套件 | 測試數 | 是否對 C 版執行 |
|---|---|---|
| 單元測試（`cargo test --lib`） | 95 | 否——內部 API |
| `cli_parity` | 18 | 是 |
| `http_parity` | 21 | 是 |
| `ws_parity` | 37 | 是 |
| `tls_parity` | 4 | 是 |
| `lifecycle_parity` | 28 | 是 |
| `forward_auth` | 19 | 否——新增功能 |

108 支共用測試中，有 99 支在兩個 binary 上主張完全相同的行為。其餘九支包含下方記錄的差異，以及 C 版沒有的行為測試（`--title`、base path 正規化，以及長度超過其 29-byte 緩衝區的身分名稱）。

逐行檢查後，C 版剩餘約 12% 未覆蓋程式碼包括：記憶體配置與 `lws_write` 失敗分支、`inflate` 失敗處理、`fork`／`execvp` 失敗路徑、`SIGABRT` handler、必須使用刻意放慢的 client 才能觸發的 HTTP 部分寫入路徑，以及因下述 C 版缺陷而不可達的程式碼。若不做 fault injection，這些都不是黑箱測試能驅動的行為。

測量還有一項限制：`-u`／`-g` 降權測試確實會跑到相關選項分支，但插樁後的 binary 降權為 `nobody` 後無法寫入 profile 資料，因此該次執行不會貢獻覆蓋率數字。

## 找到的差異

同一套測試分別對兩個 binary 執行後，找出四個真實差異，每一項的處理理由列於下方。第一項是這個移植版自己的缺陷，已修正為與 C 版一致；其餘三項是 C 版的缺陷，在原版維持原狀、在這裡修掉，因此至今仍是兩者有差別的地方。

### 1. Server 曾在 client 開口前先公布自身資訊

**發現方式：**全部 `ws_parity` 測試同時在 C 版失敗。

移植版的第一個草稿在 socket 一開啟時就送出 `SET_WINDOW_TITLE` 與 `SET_PREFERENCES`。C 實作會等瀏覽器先送出開場 frame `{"columns":…,"rows":…,"AuthToken":…}` 才傳送任何內容，因為這些訊息是由 `spawn_process` 排程的 writable callback 寫出。

**處理方式：修正移植版，使其順序與 C 版一致。**這不只是顯示問題——window title 含有完整命令列，因此原本的順序會在 `AuthToken` 尚未驗證前，就把命令列洩漏給任何僅僅開啟 socket 的 client。

與 C 版對齊後封住了驗證前的漏洞，但沒有消除根本暴露面：即使在 C 版中，任何合法開啟 session 的 client 仍會收到完整命令列。因此移植版新增 `--title`，直接取代 server 公布的 title，讓可能帶有 script 路徑、host 或 key 的命令不會抵達瀏覽器。這與既有的 `-t titleFixed=…` client 選項不同；後者只是等真正的 title 已經傳過網路之後，才改變瀏覽器畫面顯示的內容。

### 2. 正常離開時，瀏覽器收不到 close code 1000

**發現方式：**`ws_parity::a_clean_exit_closes_with_code_1000`。

前端使用 `if (event.code !== 1000)` 判斷是否重新連線。C 程式碼原本有意配合：它呼叫 `lws_close_reason(wsi, 1000)`；但又在同一次 writable callback 中立刻回傳 `1`，使 libwebsockets 直接中斷連線，而沒有完成 close handshake。從 wire 上觀察，C 版 session 總是以 `ResetWithoutClosingHandshake` 結束，所以即使使用者輸入了 `exit`，瀏覽器仍會看到 1006 並提供重新連線。

**處理方式：移植版會完成 close handshake。**正常離開送出 1000；其他情況則不送 close frame、直接中斷，瀏覽器會將其回報為 1006。對 C 參考版執行時會跳過此測試，理由記錄在 assertion 上。

### 3. `PAUSE` 沒有作用

**發現方式：**`ws_parity::pause_stops_output_and_resume_restarts_it`。

```c
void pty_pause(pty_process *process) {
  if (process->paused) return;      /* paused is always true */
  uv_read_stop(...);                /* never reached */
}
```

`process->paused` 在 `pty_spawn` 中被設為 `true` 一次，之後再也沒有被賦值，因此 `pty_pause` 永遠提前 return，`uv_read_stop` 也成為不可達程式碼。換句話說，由 client 驅動的流量控制在 C 版從未真正運作；跟不上輸出的 client 沒有辦法要求 server 放慢速度。

**處理方式：移植版實作協定所描述的流量控制。**終端輸出會通過有界 channel，因此暫停或緩慢的 client 會阻塞 reader task，kernel 的 PTY buffer 進一步對 child process 施加 backpressure。對 C 參考版執行時會跳過此測試，理由記錄在 assertion 上。

### 4. 未指定 group 的 `--socket-owner` 會被默默忽略

**發現方式：**在補上選項短格式覆蓋率時，由 `lifecycle_parity::a_socket_owner_without_a_group_sets_only_the_user` 發現。

C 版的 `-U daemon:daemon` 能正常運作，socket 最後會是 `srw-rw---- 1 1`。若使用 `-U daemon`、省略 group，結果則是 `srwxr-xr-x 0 0`：libwebsockets 無法解析字串，接著放棄整個權限設定步驟，因此 `chown` 與原本無條件執行的 `chmod 0660` 都不會發生。socket 只會留下 process umask 決定的權限。

因此，操作人員若輸入 `-U ttyd` 而非 `-U ttyd:ttyd`，會得到一個正常啟動、沒有異常 log，卻完全沒有套用要求權限的 server：指定的 user 並未擁有 socket，group 也無法存取。實際暴露程度取決於 umask（見下方說明），但無論如何，這個選項都等於被默默忽略。

**處理方式：移植版會套用 user 部分，並依然強制設定 mode。**group 缺省代表「不要改 group」，而不是「什麼都不做」。對 C 參考版執行時會跳過此測試，理由記錄在 assertion 上。

## 對 C 版的其他觀察

這些結果來自差異測試，但移植版本身沒有改變它們，因此另列於此：

- **生命週期極短的 process 可能遺失輸出。**使用 `ttyd -a -W echo` 時，C 版大約五次只有三次能送達輸出；突然 teardown 會與最後一個 frame 競速，而 TCP reset 會丟棄 client 尚在 buffer 裡的資料。Rust 版因為會正常關閉，在同樣的執行中表現穩定。兩支測試後來改成讓 child 短暫存活，確保測的是參數傳遞，而不是 shutdown timing。

## 刻意改善之處

除了上述四項差異，移植版也刻意做出以下改變：

- **Basic auth 使用 constant-time 比較**（`subtle::ConstantTimeEq`）。C 版使用 `strcmp`，遇到第一個不同的 byte 就回傳，因而洩漏猜測的 credential 有多少前綴正確。
- **`Basic` scheme 名稱不分大小寫比對**，符合 RFC 7617。C 版使用 `strstr(buf, "Basic ")`，會拒絕符合規範的 `basic`。
- **`-t key=value` 保留第一個 `=` 之後的全部內容。**C 版遇到第二個 `=` 還會再切一次並默默丟掉後續內容，所以 `-t token=a=b=c` 最後只會存下 `a`。
- **404 response 與 libwebsockets 逐 byte 相同**，因此任何爬取 error page 的程式都不會看到變化。
- **Credential 永遠不會進入 log。**C 版會在啟動 banner（`server.c`）中印出 base64 credential，WebSocket handshake 失敗時也會回顯送來的 token（`protocol.c`）。Base64 是編碼，不是加密；這兩行會把可逆的 `user:password` 放進所有收集 stdout 的系統。移植版只會報告 basic auth 已啟用而不印出值，並以長度描述 token mismatch。`lifecycle_parity::the_credential_never_reaches_the_log` 會固定這項行為。
- **整數選項拒絕尾隨垃圾字元與八進位 literal。**C 版使用 `strtol(…, 0)` 解析，因此會把 `-p 80abc` 接受為 port 80，也會把 `-p 010` 讀成 8。移植版要求完整值必須是十進位數字（或帶 `0x` 前綴），否則以標準的 `invalid value for …` 訊息結束。
- **`--base-path` 會正規化並驗證。**沒有開頭 slash 的值會被接受並正規化（`mounted` → `/mounted`），不會原封不動送入 router；包含 `{`、`}`、`?` 或 `#` 的值會被拒絕，因為這些是 route-matching 語法，可能默默把 endpoint 變成 wildcard capture。
- **明確尊重 `gzip;q=0`。**兩個版本原本都是檢查 `Accept-Encoding` 是否「包含」`gzip`（`http.c` 中的 `strstr`），因此 `gzip;q=0` 也會被判定為接受 gzip；但 RFC 9110 用它表示「不要傳 gzip 給我」。client 因而收到自己才剛聲明無法解碼的壓縮 body，瀏覽器會顯示原始 deflate stream。移植版會將 header 解析為 token 並尊重零權重。兩版以十種 header 形式實測後，差異僅限於 `gzip;q=0`（現在不壓縮）與 `GZIP`（現在壓縮，因 RFC 9110 規定 coding name 不分大小寫）。為了與 C 版一致，`*` 仍刻意**不**視為接受 gzip；任何 client 都能接受未壓縮 body，因此沒有必要在此製造差異。
- **Log timestamp 使用 UTC**，C 版則印出 local time。這是刻意的：未設定 `TZ` 的 container 本來就是 UTC，而 UTC 也更容易跨 host 對照。

新增兩個選項，既有選項沒有移除：上面提到的 `--title`，以及 `--auth-url` 與其搭配選項；詳見 [README.zh-TW.md](README.zh-TW.md)。

其中一個選項對應到不同機制。C 版的 `--srv-buf-size` 設定 libwebsockets 的 per-thread service buffer；hyper 沒有對等調整旋鈕，因此移植版把它套用到最接近且可觀察的行為：每次從 PTY 讀取、並放入單一 WebSocket frame 的終端輸出上限。預設值仍為 4096，與 C 版相同；`lifecycle_parity::the_send_buffer_size_bounds_one_output_frame` 會固定這項行為。

## 覆蓋率測量暴露的缺口

測量 C 版的覆蓋率不只產出一個數字，也指出原測試套件沒有觸及哪些部分；其中兩處後來證實是移植版的真實問題，而不只是缺少測試：

- **`--ping-interval` 被解析後便遭忽略。**這個選項原本完全沒有效果，因此 idle terminal 會被任何設有 idle timeout 的 reverse proxy 中斷。移植版現在會依該 interval 傳送 WebSocket ping；若 peer 在 `interval + 7` 秒內沒有回應便中斷，與 C 版的 retry policy 相同。
- **SIGTERM 之後 child process 仍繼續執行。**終止 server 不會清掉 live session；又因為每個 child 都領導自己的 process group，kernel 也不會一併向它們送 signal，所有 terminal 都會成為 orphan 繼續存活。現在 shutdown 會傳播至 live session，由 session 在 process 結束前 signal 自己的 child。

兩項都有也能在 C 版通過的測試覆蓋。

## 這個移植版曾帶入、後來修正的缺陷

記錄這些項目，一方面是因為讀者有權知道第一版落地後哪些部分必須修正，另一方面是每一項都指出上述測試方法曾看不見什麼。

- **Forward-auth cache 可能重播先前的判定。**Cache key 包含 path 與 operator 列出的 request header，但 subrequest 同時還攜帶 method 與整組 `X-Forwarded-*`；因此，一個 request 得到的允許判定可能在沒有再次詢問 endpoint 的情況下，被重用於原本會遭拒絕的另一個 request。差異測試無法抓到這件事：C 版沒有 forward auth，自然沒有可以產生差異的對照實作。修正方式是讓 cache key 與 outgoing header 由同一個 structure 衍生，使兩者不可能各自漂移。
- **降權後仍保留 supplementary group。**`setgid` 不會動到該清單；libwebsockets 會呼叫 `initgroups`，移植版原本沒有。既有 `--uid` 測試雖然會在兩個版本上執行，卻只 assertion `id -u`，深度不足以發現問題。現在改用 `initgroups`／`setgroups` 修正；測試會檢查完整 group list，並自行建立 marker group，不再依賴執行環境剛好如何設定。
- **`--url-arg` 沒有做 percent decode。**實作時假設 libwebsockets 交付 raw fragment，但實測發現它會解碼。原測試只用 `first` 與 `second`，無論有無解碼看起來都一樣。修正後，測試改用包含空白與非 ASCII 字元的值。
- **`--srv-buf-size` 被解析後從未讀取**；同時 WebSocket access log 把每個 client 都回報成 `unix`，因為 handler 建立了預設 `ConnInfo`，沒有讀取 accept loop 記錄的那份。兩者均已修正並補上測試。
- **高負載下 shutdown 會留下 terminal。**終止 server 時，原本仰賴每個 session task 自行醒來並 signal 其 child；平行負載下 task 可能錯過時機，而每個 child 又領導自己的 process group，沒有其他東西會 reap 它。現在 server 會在結束前自行 signal 已註冊的 child。
- **`begin_shutdown` 可能默默丟掉 signal，使 SIGTERM 遭忽略。**它呼叫 `watch::Sender::send` 後忽略 error；但若當下沒有 receiver 訂閱，該方法不只回傳 error，還會讓 value 維持不變。由於 `wait_for_shutdown` 每次呼叫才訂閱，中間存在一段 window（accept loop 正在 branch body 內且沒有 live session），signal 可能在此消失，server 便繼續執行。`send_replace` 會無條件記下 value。這是在移除無人讀取的 `accepting` flag 時發現的：把原測試改為檢查 accept loop **實際** select 的機制後，一支原本通過卻什麼都沒證明的測試，終於因真實原因失敗。
- **沿用的 `force_exit` flag 從未被讀取。**C 版會在兩處檢查它：第二次 signal 升級為立即離開，以及 child 結束後終止 process。移植版透過其他方式實作這兩項行為（第二次 signal 使用 `select!`，child 則明確 wait），所以設定 flag 完全沒有作用。映照真實機制、實際上卻無人使用的殘留 state 比完全沒有更糟，因為讀程式的人會誤以為行為由它控制。現已移除。
- **過大的 `--srv-buf-size` 會在第一個 connection 到來時殺死 server。**該大小會為每個 session 配置一次，原本沒有上限：`-f 9999999999999` 可以正常啟動，卻在第一個 client 連線時死亡。C 版使用相同參數仍能服務（RSS 成長至 1.29 GB），所以這同時是 typo 可造成的 denial of service 與相對原版的 regression。現在上限 clamp 為 16 MiB，並明確回報，而不是默默套用。
- **`--auth-url` 與 `--auth-method` 未經驗證便接受。**Forward auth 採 fail closed，所以 URL typo 會正常啟動，直到第一個 request 才對所有流量回答 `500`；拼字錯誤因而從 startup error 變成全面 outage。兩者現在都在 parse 時檢查。
- **Query string 可以讓 connection task panic。**`decode_query_value` 以 byte offset 對 `&str` slicing 來讀 `%XX` escape，但 byte offset 不一定落在字元邊界；`?arg=%aé` 會切進 `é` 中間並 panic。Query string 直接來自 wire，因此啟用 `--url-arg` 後，任何 client 都能任意殺掉自己的 connection task。這已在執行中的 server 上重現，log 顯示 `panicked at src/ws.rs:478`。Escape 現在改由 byte slice 解碼。
- **當 `-i <name>` 指定的 interface 有 IPv4 address 時，`-6` 會被忽略。**v6 branch 會落入 v4 branch，而 v4 address 通常先被列舉，所以常見情況會默默綁到 IPv4。現在會跳過非 v6 entry；若 interface 完全沒有 IPv6 address，則回報 error。
- **`accept` 失敗會讓 loop 以滿 CPU 空轉。**兩個 accept loop 都丟棄 error 並立刻繼續。Descriptor 耗盡造成的 `EMFILE`／`ENFILE` 並非 transient，所以下一次 `accept` 會立即再度失敗；loop 因而 busy-spin 一個 core，還會餓死原本能關閉 connection、釋放 descriptor 的 runtime。現在會記錄失敗並 back off。
- **UNIX socket 在 `bind` 與 `chmod` 之間短暫帶著 process umask 決定的權限。**現在 bind 會在已經拒絕其他所有人的 umask 下發生，mode 不再取決於 `chmod` 多快完成。
- **PTY master 在 child spawn 後才標記 close-on-exec。**從 `openpty` 到該時間點之間，任何同時啟動的其他 session 都可能 fork 並繼承這個 descriptor，讓一個 session 的 child 能讀寫另一個 terminal。現在兩端都會在任何 fork 發生前先標記；slave 仍能抵達 child，因為 `dup2` 會清除 copy 上的 flag。
- **Operator 可以重新引入 forwarded header。**若在 `--auth-request-header` 中列出 `x-forwarded-for`——在不知道它由系統合成時很自然——會同時傳送 client 提供的值與實際觀察到的 peer，而且 client copy 排在前面，破壞「只轉送觀察到的 address」這項保證。現在會丟棄與 synthesized set 衝突的 client copy。
- **Exit receiver 完成後仍可能再次被 poll。**Branch guard 原本檢查 `exit_info.is_none()`，但 sender 被 drop 時會回傳 `Err` 且 `exit_info` 仍為 empty，因此 guard 會持續為 true；`oneshot::Receiver` 又不是 fused，下一次 poll 便 panic。現在 guard 改為判斷 receiver 是否已經完成。
- **在 `-d 0` 下看不到 fatal error。**Startup failure path 使用 `tracing::error!`，但 `-d 0` 不會安裝 subscriber，因此 process 會毫無訊息地以 1 結束。現在會直接寫入 stderr。
- **`--check-origin` 接受 C 版拒絕的 origin，也拒絕 C 版接受的 origin。**`check_host_origin` 不論 scheme 為何都會從 origin 移除 `:80` 與 `:443`，並且對 scheme 本身做區分大小寫的 exact match。移植版原本只移除 scheme 自己的 default port，所以 `https://host:80` 對 `Host: host` 會被拒絕，而 C 版會接受；同時移植版比對前會把 scheme 轉為小寫，所以 C 版會拒絕的 `HTTP://` 甚至 `ftp://` 反而被接受。後半部尤其重要：安全控制比參考實作**更寬鬆**，是錯誤的差異方向。兩者現在由會對兩個 binary 執行的測試固定，涵蓋人工比較過的 17 種 origin 形式。
- **HTTPS redirect 可能產生 client 無法使用的 URL。**沒有可用 authority 的 request——例如不要求 `Host` 的 HTTP/1.0——會產生 `Location: https:///token`。現在 authority 優先取自 `Host`；HTTP/2 與 absolute-form request 則 fallback 到 URI 自身的 authority；兩者都沒有時改回 `400 Bad Request`。（C 版會直接斷線，不回覆任何內容。）
- **Terminal input 可以無上限排隊。**Child 讀取緩慢或完全不讀時，kernel PTY buffer 會填滿並阻塞 writer thread，client 後續持續送出的所有內容都堆在 server memory。實測：向不讀取的 child 送 15 MiB input，RSS 增加 6.5 MB，沒有任何限制；C 版因 libwebsockets 套用 read flow control，吸收 179 MiB 後成長仍不到 1 MB。現在 session 在 outstanding data 達 4 MiB 後停止讀取 socket，讓 backlog 由 TCP backpressure 限制，而非記憶體。刻意採用 read gating 而不是在讀取內等待，因為 session 以同一個 `select!` 驅動雙向流量；若在其中等待，也會停止排出 child output，讓同時讀寫的 child 自我卡死。這不是假設：第一版修正確實採取阻塞方式，使用 `cat` 的測試也正因此 deadlock。
- **UNIX domain socket 保留 process umask 決定的權限。**libwebsockets 在 bind 後立刻對它 `chmod 0660`；移植版原本沒有，也略過對 `-u`／`-g` 的 `chown`。這不是任何測試找出的，而是逐行對照 `serve.rs` 與 `server.c`、並對兩個 binary 並列執行 `strace` 時發現。原 socket 測試在 root 下執行，只 assertion `uid == 0`；無論程式有沒有做事都會成立。現在會檢查 mode，並 chown 給不同於測試自身的 user。
- **一支守護 input backlog 的測試，只在它被寫出來的那台機器上會過。**它繞過 `ws::session` 實際套用的 gate 一路寫下去，並斷言 8 MiB 之內就會撞到上界——那量到的是某台主機的 line discipline 在停止接收前會吞掉多少，而不是這個上界本身。它從 `a827e3d`（PTY 由阻塞 writer thread 改為 readiness 驅動 I/O）開始變紅，之後的十一個 commit、五個星期都紅著沒有被發現，包含這個移植版被合併的那一次。沒有人看的紅燈比缺少測試更糟，因為它照樣被算進測試清單裡。上界本身從未失效：照 gate 的做法模擬，Linux 上 backlog 峰值正好停在 4 MiB；macOS 則根本不會累積，因為該核心是丟棄多餘資料而不是拒絕寫入，實測送出超過 512 MiB 仍沒有任何 outstanding。測試現在改為斷言兩邊都成立的不變量：只要呼叫端在 gate 前停手，排隊量就不會超過上界再加一個 chunk。
- **Forwarded identity 被默默截斷為 29 bytes**，照搬 C 版的 buffer。影響比表面更嚴重：兩個共享 29-byte prefix 的 account 會被折疊成同一個 `TTYD_USER`。限制現已移除，名稱會完整傳遞。（C 版會直接拒絕這類名稱的 WebSocket upgrade；測試套件現在也會在 C 端 assertion 此行為。）

## 已知缺口

**Windows 尚未移植。**C 實作透過 ConPTY 支援 Windows（`src/pty.c`、`#ifdef _WIN32`）。Rust 版只原生實作 Unix PTY 路徑，使 `setsid`、controlling terminal acquisition、process-group signalling，以及 `128 + signal` 的 exit convention 能與原版完全一致。Windows backend 可以在相同 `pty` module interface 後方獨立加入；這次選擇暫不提供，而不是交付未經測試的實作。

**Unix 路徑在 Linux 與 macOS 上都經過實測**，兩邊測試套件都全綠：222 支測試分別在一台 arm64 macOS 15.5 主機與 `rust:1.92-slim` container 上通過。達成它需要兩處編譯修正與四處測試修正，都沒有動到行為。`initgroups` 的 base group 參數在 Linux 是 `gid_t`、在 Apple 平台是 `int`，`setgroups` 的數量參數則分別是 `size_t` 與 `int`，因此移植版原本在 macOS 上完全無法編譯。另有四支測試帶著對執行主機而非對程式碼的假設：loopback 裝置在 Linux 叫 `lo`、其他平台叫 `lo0`（兩支）；`--browser` 呼叫的系統開啟程式是 `xdg-open` 或 `open`；殘留 socket 那支只等路徑存在，但它自己剛寫下的殘留檔案本來就存在；`-6` fallthrough 那支把 `lo` 當成沒有 IPv6 位址的介面，然而只要 loopback 帶有 `::1` 這個前提就不成立——它在 Linux 上其實也一直失敗，敗的原因是這個，不是它要守的 fallthrough。

這片綠燈有兩點必須說明，因為 skip 與 pass 看起來一樣。`-6` fallthrough 那支現在會去找一個有 IPv4 而沒有 IPv6 的介面，主機上沒有就跳過；而在每個介面都帶 link-local `fe80::` 的機器上，那是大多數情況。它會把跳過這件事印出來，而不是安靜地通過。另外，這套測試是人工執行的：`.github/` 底下的 workflow 建的是 C 專案，CI 裡沒有任何東西會跑 `cargo test`。BSD 系統未經測試。

C 版 feature matrix 的其他部分皆已實作並覆蓋：全部 30 個 command-line option、四個 HTTP endpoint、八種 WebSocket message type、三種 authentication mode、帶 client-certificate verification 的 TLS、UNIX domain socket、privilege dropping，以及 `--once`／`--exit-no-conn` lifecycle rule。每個 option 至少有一支測試 assertion 可觀察結果，因此這裡的「已實作」指的是確實執行過，而不只是完成 parser。

其中一個選項另有 caveat，特別說明是避免上段過度陳述。`-6` 有 unit test 固定其選擇的 address，也有 integration test 透過 `[::1]` 提供真實 request；但驗證用 container 沒有 IPv6 stack（`bind` 回傳 `Address family not supported by protocol`），所以該 integration test 在此環境是 skipped，而非實際通過。此選項只有在啟用 IPv6 的 host 上，才完成 end-to-end 證明。

## 瀏覽器驗證

其餘測試套件使用 synthetic client 與協定互動，能證明 wire format，卻不能證明出貨的 frontend 能與之配合。`browser-check.py` 透過 Playwright 驅動真實 Chromium 與真實 xterm.js bundle，並對兩個版本檢查：

- frontend 能載入，xterm.js 能 mount；
- 輸入的 keystroke 能抵達 shell（透過 shell 建立的 file 驗證；xterm.js 會 render 到 WebGL canvas，無法從 DOM 讀取 terminal text）；
- `TERM` 能抵達 child，viewport resize 能更新其 `winsize`；
- color、CJK 與 box-drawing 能 render（screenshot）；
- full-screen program（`vi`）能透過 alternate screen 完整 round-trip；
- 沒有未捕捉的 frontend error，session 全程不中斷；
- 設定 `TTYD_BROWSER_TLS=1` 時，使用產生的 CA 對 HTTPS 執行相同流程，並 assertion `location.protocol === "https:"`。

其中兩件事是 protocol-level 測試無法發現的：`--title` 必須以真實 browser tab 檢查；WebSocket 會讓 page 持續 busy，所以 `networkidle` 永遠不會發生。

這也產生過一項值得記錄的錯誤指控。`vi` 對 C 版連續失敗三次、對 Rust 版則通過，看起來像 C 版缺陷。比較 raw protocol output 後，兩個版本其實都送出逐 byte 相同、包含 `?1049h` 的 3370-byte response。真正原因是**前一次**執行留下 `.swp` file，使 vim 顯示 prompt 而非直接開啟。Harness 帶著跨執行 state；修正方式是每次使用 fresh directory。

## 效能

兩個版本都使用 Release build：C 版使用 `-O3 -DNDEBUG`，Rust 版使用 LTO、單一 codegen unit，並移除 symbol。執行順序交錯為 C、Rust、C、Rust，而不是先跑完其中一版，讓 machine drift 同時落在兩者，而不會只影響後跑的版本。每個數字都是一台原本 idle 的 4-core 機器上五輪執行的 median；每列也列出所有 round 的完整 range，因為小於 spread 的差距不能視為差異。

Harness 為與本檔一同 commit 的 `bench.py`，因此表格可重新執行，而不必照單全收。

| 測量項目 | C（median） | C range | Rust（median） | Rust range | 判定 |
|---|---|---|---|---|---|
| 啟動至開始監聽（ms） | 4.3 | 4.0–5.6 | **2.7** | 2.6–2.9 | Rust；range 不重疊 |
| Baseline RSS（kB） | **5108** | 5068–5116 | 5256 | 5196–5280 | C，但只少 3% |
| **每個 idle session 的 RSS（kB）** | **17.3** | 17.3–17.4 | 82.7 | 80.3–83.2 | **C，4.8×** |
| HTTP `/token`（req/s） | 4582 | 3886–4786 | 4862 | 4402–5147 | range 重疊——不下判斷 |
| Terminal session（open+close/s） | 163 | 152–173 | **195** | 179–206 | Rust；range 不重疊 |
| Terminal output（MB/s） | 76.7 | 47.9–83.6 | **92.3** | 85.8–92.8 | Rust；range 不重疊 |
| 每傳送 1 MB 的 CPU（ms） | 8.6 | 8.4–9.2 | 8.5 | 8.1–9.0 | range 重疊——相同 |

其中兩列刻意不下結論。Request rate 由本身就是瓶頸的 Python client 產生，且 range 彼此重疊；每 byte CPU 是從 `/proc` 讀出的真實測量，結果確實不分高下。只有 range 不重疊的四列足以支持判斷。

**這些數值應視為下限，不是該機器的最佳成績。**一個完整 round 會同時降低兩個版本的 throughput，而且效果會累積：單獨連跑三次 `session_rate`，C 穩定在 376–416/s，移植版為 762–870/s；若將它排在七個步驟中的第五步、完整掃過五輪，median 便降為 163 與 195。兩個版本一起受到壓抑，ratio 的變化遠小於 absolute figure——這正是交錯執行的用意，也是每列都附上 spread 的原因。累積的具體因素尚未確認：不是 leaked shell（兩版各跑 500 個 session 後，`ps` 都沒有殘留），也不是 lingering `TIME_WAIT` socket（idle 時為五個）。這裡把它列為 open question，避免留給讀者自行踩到。

因此，absolute number 不能與此表較早的 revision 相比；舊版還使用了會在不知不覺間替機器增加負載的 harness，詳見下方。

這張表較早的 revision 呈現完全不同的結果：throughput 差異落在 noise 內，而且 C 版每送一個 byte 少用 23% CPU。那是在 PTY 停止為每個 session 使用三個 OS thread——reader、writer 與 reaper——之前測量。移除它們（見下方項目）不只降低記憶體，也讓 throughput 與 CPU efficiency 從「C 領先」變成「Rust 領先或相同」；每個 session 三個 thread 乘上 client 數量，帶來的是 scheduler pressure，不是有效工作。

### 為什麼 throughput 會呈現這樣的結果

在機制尚未釐清前，「Rust 比較快」不能算結果，因此兩個 range 不重疊的項目都經過追查，而非直接推定。兩項原因最後都與 *idle time* 有關，不是 Rust 產生了更好的 machine code。

**Terminal output（92.3 vs 76.7 MB/s）。**每 byte cost 相同，差別在 CPU utilization。在同一台 4-core 機器傳送相同 generated stream：

| | Throughput | Server CPU | 每 MB/s 的 %CPU |
|---|---|---|---|
| C | 78.3 MB/s | 79.5%，全在單一 thread | 1.02 |
| Rust | 93.4 MB/s | 89.7%，分散至四個 worker，每個 ≤30% | 0.96 |

關鍵數字是 C 的 **79%**。它的單一 event-loop thread 並未飽和，仍有五分之一時間 idle，因此瓶頸不是 client 或 generator，而是結構。`src/pty.c` 的 `read_cb` 在第一行呼叫 `uv_read_stop`，把 chunk 放入 `pss->pty_buf`；直到 writable callback 內的 `lws_write` 回傳後，才呼叫 `pty_resume`。任一時間永遠只有一個 chunk in flight：socket write 時不讀 PTY，讀 PTY 時也不寫 socket。`strace -c` 證實此形狀——C 每次 PTY read 會執行 **2.0 次 `epoll_pwait`**（每 MB 506 vs 253），相當於每個 chunk 需要兩個完整 event-loop turn。

移植版使用獨立 task 讀取 PTY，送進 depth-1 `mpsc`；因此 chunk *N* 正在 framing 與 writing 時，chunk *N+1* 已經開始讀取。相同 trace 顯示：每次 read **1.15 次 `epoll_wait`**（每 MB 294 vs 256）。勝出的原因完全是 overlap，但並非免費：cross-thread handoff 會表現為每 MB 228 次 `futex`，也正因如此，每 byte CPU 最後只是打平，而非勝出。

值得記錄一個先前的錯誤猜測：每個 chunk 呼叫一次 `uv_read_stop`／`uv_read_start`，看起來應該各造成兩次 `epoll_ctl` syscall。但 trace 顯示兩個版本都是 **零次** `epoll_ctl`；libuv 會在下一輪 event loop 前，於 watcher queue 中合併 stop 與 start。Ping-pong 的成本是額外一次 loop turn，不是重新 arm。

**Session churn（195 vs 163 open+close/s）。**直接測量 idle 到十個 concurrent session：C 從 2 個 thread 增至 12 個，移植版維持 5 個。C 每個 session 都為 blocking `waitpid` 建立真實 pthread（`pty_spawn` 中的 `uv_thread_create(&process->tid, wait_cb, process)`），並於 `process_free` 中 `uv_thread_join`。每個 session 因此都付出一次 `clone`、stack mapping 與 join handshake。移植版則建立三個 tokio task。這與移植版曾帶入再修正的缺陷形成對稱：第一版每個 session 使用**三個** OS thread，在兩個測量項目都比 C 慢。把 C 的一個降為零，是同一個 lever 再往前推一格。

### 這張表在變正確前，曾經錯過兩次

兩件事都值得記錄，因為兩者都產生了非常像真的數字。

**Harness 本身就是負載。**`bench.py` 對每個 server 傳送 SIGTERM，十秒後仍未結束便升級為 SIGKILL。實測兩個版本收到 SIGTERM 後，都能在約 0.01 秒內 reap terminal 的 process tree；但被 SIGKILL 的 server 永遠不會執行該路徑，而 `-W sh -c 'while true; …'` 會留下失去 parent、持續執行的 `dd | tr` loop。每個 orphan 都增加 load，load 讓下一次 shutdown 變慢，較慢的 shutdown 又撞上 escalation。五輪結束後，機器上留下**二十六個仍在執行的 generator，4-core 機器 load average 達 33**；輸出隨 round 劣化，呈現得像一項測試結果：C 的 terminal-output spread 從 12.2 到 71.5 MB/s，session rate 則減半。

Signal server 的 process group 無法解決，值得明說，因為那是最直覺的第一次嘗試。ttyd 會以 `setsid` 加 `TIOCSCTTY`，讓 terminal 擁有自己的 session——對 terminal server 而言完全正確——所以 shell 不在 server group 中，對該 group 發送的 signal 都到不了它。現在 harness 會在每個 server 結束後掃描存活的 generator；先等三秒，避免把仍在正常退出的 process 誤判為 leak；機器上若一開始就有 generator 則拒絕啟動；並印出必須 reap 的數量。

有一個結論刻意**沒有**宣稱：加入 sweep 後，SIGKILL escalation 完全不再觸發，但 orphan 仍會出現——每輪約四個，只在機器已工作一段時間後發生；單獨執行相同兩項測量則從未出現。Escalation 是產生它們的一條路徑，但不是唯一一條，第二條路徑仍未確認。這張表能說的是：產生表格的那次執行清掉了自己留下的 stray，結束時沒有任何殘留。

**Benchmark 在 busy machine 上執行。**第一版表格的測量期間，同時有一個八分鐘 browser soak 在執行。那些數字碰巧通過了之後的 clean rerun，但當時仍不該發布。

兩項錯誤有相同形狀：測量默默把 measurer 本身也算了進去。因此 harness 現在會 assertion，而不是 assumption；也因此它會跟著 commit 在 repository 裡。

**C 的每 session 記憶體用量仍勝出，但差距現在是 4.8×，不是 12×。**主要成本已找出並移除。

拆解每個 connection 在修改前後的 RSS：

| 階段 | 修改前 | 修改後 |
|---|---|---|
| 一般 HTTP connection | 64 kB | 64 kB |
| WebSocket 已 upgrade、尚未開啟 terminal | 187 kB | 62 kB |
| 完整 terminal session | 219 kB | 104 kB |

WebSocket upgrade 增加的 125 kB 來自單次 128 KiB allocation。在帶 debug symbol 的 build 上使用 `gdb` 對 `mmap` 設 catchpoint，追到 `tungstenite::FrameCodec::new` 呼叫 `BytesMut::with_capacity`：那是 **read** buffer，其 `read_buffer_size` 預設為 128 KiB，socket upgrade 時便 eager allocation。（先前曾嘗試縮小 *write* buffer，結果沒有變化；原因是 write buffer 採 lazy allocation。兩者是獨立設定，只有 read buffer 會 eager allocation。）Read buffer 只攜帶 client-to-server 流量——keystroke、resize message、opening frame——因此改為 16 KiB；較大的 paste 仍可運作，因 `BytesMut` 會按需成長。這已透過兩個版本使用 `cat` echo 一個 1 MB single frame 驗證。

上述拆解與表格中的 82.7 kB/session 衡量的是不同事物，不必相等：前者從 `smaps` 讀取單一 connection 自身的 mapping；後者則把 resident growth 除以二十五個 simultaneous session。Marginal session 比第一個便宜，因 allocator 會重用已經 fault in 的 arena，所以 amortized figure 會低於 single-connection number。

剩餘成本是每個 connection 在成為 WebSocket 前就會產生的約 64 kB——hyper 自己的 per-connection buffer。這個目標較小，又直接影響 HTTP throughput，因此沒有只憑假設繼續追逐。Terminal session 本身現在只在此基礎上增加約 40 kB。

## Soak test

十分鐘內，八個 concurrent client 持續 connect、stream、disconnect，每 30 秒取樣一次 server：

| | 開始（t=60s） | 結束（t=600s） |
|---|---|---|
| RSS | 10 180 kB | 10 692 kB |
| Open descriptor | 42 | 42 |
| Thread | 29 | 29 |

共 792 個 session、11.2 GB terminal output，沒有 error。RSS 全程在 9.9 MB 至 11.1 MB 間震盪，沒有趨勢；descriptor 與 thread 完全沒有變化。

第二次執行針對 input path，而非 output path：client 向完全不讀取的 child 大量送入 terminal input，遭中斷或阻塞後重新連線並重複。20 輪中，RSS 在 12.2 MB 至 13.1 MB 間震盪，最後停在 12.6 MB；backlog ceiling 對每個 session 都有效，session 結束後也能回收，不會隨 reconnect 逐輪上升。

第一次做十分鐘測量時，報告顯示 server 在三分鐘後 freeze。問題其實出在 harness：它只把 server stderr 讀到 port line 出現，之後便停止，log 最後塞滿 64 kB pipe buffer，server 因 `write()` 阻塞。之所以記錄，是因為在檢查 stack 前，這個失敗看起來與 server defect 完全相同；它與上方 vi false alarm 屬於同一類錯誤。

## 經檢查後不採用的修改

Review 提出的意見不全是必須修正的缺陷。下列項目與 C 版比對後維持原狀，因為改動反而會製造差異，而非修正錯誤：

- **`--browser` 會等待啟動的 process。**`open_uri` 使用 blocking `status()`，所以停留在 foreground 的 handler 會延遲 accept loop。C 版執行完全相同的動作——`utils.c` 中先 `fork` 再 `waitpid`——因此這是 parity，不是 regression；實務上 `xdg-open` 會自行 detach。
- **Display probe 只會偵測 X11。**沒有 Xwayland 的 Wayland session 不會有 `xset -q`，所以 `--browser` 在該環境會默默不做事。C 版同樣執行完全一致的 probe（`system("xset -q > /dev/null 2>&1")`）。這值得在兩版 upstream 一起修正，卻不值得只在此移植版製造差異。

## 更正：UNIX socket 的 mode 實際控制什麼

本文件較早版本與 `serve.rs` 的註解曾把 mode `0755` 的 socket 描述為「所有人都能連線」，並聲稱任何 local user 都能透過它開啟 terminal。**這是錯的**，而且同一錯誤在 review 發現前曾重複出現於數處。

連線至 UNIX domain socket 需要對 socket file 具有**寫入**權限。以 unprivileged user 直接測量：

| Mode | 其他 user 能否連線 |
|---|---|
| `0755` | 否——`EACCES` |
| `0775` | 否——`EACCES` |
| `0777` | **是** |
| `0660` | 否——`EACCES` |
| `0666` | **是** |

因此，在常見的 `0022` umask 下，未經 `chmod` 的 socket 會是 `0755`，原本就不允許其他 user 連線；對「能否連線」而言，`0755` 其實比 C 版設定的 `0660` 更嚴格，因後者刻意開放給 group。與 `0660` 對齊的理由是 parity、實現 `--socket-owner` 原本要授予的 group access，以及確保 mode 不取決於 server 剛好繼承的 umask。以 `umask 0` 啟動的 process 會 bind 出 `0777`，那才是真的向所有人開放。

缺陷本身是真的，但原本的 impact statement 誇大了。這裡選擇留下更正，而不是悄悄修改，因為一項事後證實錯誤的安全主張，作為修正紀錄比直接刪除更有價值。

## 相依套件

`cargo audit` 對 RustSec database（1169 則 advisory）檢查 `Cargo.lock` 的 222 個 crate，結果為**沒有 vulnerability**。另有一項 informational warning：`rustls-pemfile` 2.2.0 被標記為 unmaintained（RUSTSEC-2025-0134）。它只用來 parse `--ssl-cert`、`--ssl-key` 與 `--ssl-ca` 指定的 PEM file——這些是 operator 提供的 local file，不是 network input。

沒有執行 Trivy：此環境的 proxy 將 GitHub access 限制在 session 自己的 repository，因此 installer、release API 與 apt repository 都無法存取。這是本報告的缺口，不是乾淨的檢查結果。
